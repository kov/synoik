// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

//! Window layout logic.
//!
//! Synoik implements scrollable tiling with dynamic workspaces. The scrollable tiling is mostly
//! orthogonal to any particular workspace system, though outputs living in separate coordinate
//! spaces suggest per-output workspaces.
//!
//! I chose a dynamic workspace system because I think it works very well. In particular, it works
//! naturally across outputs getting added and removed, since workspaces can move between outputs
//! as necessary.
//!
//! In the layout, one output (the first one to be added) is designated as *primary*. This is where
//! workspaces from disconnected outputs will move. Currently, the primary output has no other
//! distinction from other outputs.
//!
//! Where possible, synoik tries to follow these principles with regards to outputs:
//!
//! 1. Disconnecting and reconnecting the same output must not change the layout.
//!    * This includes both secondary outputs and the primary output.
//! 2. Connecting an output must not change the layout for any workspaces that were never on that
//!    output.
//!
//! Therefore, we implement the following logic: every workspace keeps track of which output it
//! originated on—its *original output*. When an output disconnects, its workspaces are appended to
//! the (potentially new) primary output, but remember their original output. Then, if the original
//! output connects again, all workspaces originally from there move back to that output.
//!
//! In order to avoid surprising behavior, if the user creates or moves any new windows onto a
//! workspace, it forgets its original output, and its current output becomes its original output.
//! Imagine a scenario: the user works with a laptop and a monitor at home, then takes their laptop
//! with them, disconnecting the monitor, and keeps working as normal, using the second monitor's
//! workspace just like any other. Then they come back, reconnect the second monitor, and now we
//! don't want an unassuming workspace to end up on it.

use std::collections::HashMap;
use std::mem;
use std::rc::Rc;
use std::time::Duration;

use monitor::{InsertHint, InsertPosition, InsertWorkspace, MonitorAddWindowTarget};
use scrolling::{Column, ColumnWidth};
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::output::{self, Output};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size, Transform};
use synoik_config::utils::MergeWith as _;
use synoik_config::{
    Config, CornerRadius, LayoutPart, PresetSize, WindowingMode, Workspace as WorkspaceConfig,
    WorkspaceReference,
};
use synoik_ipc::{ColumnDisplay, PositionChange, SizeChange, WindowLayout};
use tile::{SnapshotRenderer, Tile, TileRenderElement};
use workspace::{WorkspaceAddWindowTarget, WorkspaceId};

pub use self::monitor::MonitorRenderElement;
use self::monitor::{Monitor, WorkspaceSwitch};
use self::workspace::{OutputId, Workspace};
use crate::animation::{Animation, Clock};
use crate::frame_log::AnimCauses;
use crate::gnome::{EdgeTileTarget, TileSide};
use crate::input::swipe_tracker::SwipeTracker;
use crate::layout::scrolling::ScrollDirection;
use crate::render_helpers::background_effect::BackgroundEffectElement;
use crate::render_helpers::offscreen::OffscreenData;
use crate::render_helpers::snapshot::RenderSnapshot;
use crate::render_helpers::solid_color::SolidColorRenderElement;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::RenderCtx;
use crate::rubber_band::RubberBand;
use crate::synoik_render_elements;
use crate::ui::overview_layout::ControlsLayout;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{
    ensure_min_max_size_maybe_zero, output_matches_name, output_size,
    round_logical_in_physical_max1, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

pub mod closing_window;
pub mod expose;
pub mod floating;
pub mod focus_ring;
pub mod insert_hint_element;
pub mod monitor;
pub mod opening_window;
pub mod placement;
pub mod scrolling;
pub mod shadow;
pub mod tab_indicator;
pub mod thumbnails;
pub mod tile;
pub mod workspace;

#[cfg(test)]
mod tests;

/// Size changes up to this many pixels don't animate.
pub const RESIZE_ANIMATION_THRESHOLD: f64 = 10.;

/// Pointer needs to move this far to pull a window from the layout.
const INTERACTIVE_MOVE_START_THRESHOLD: f64 = 256. * 256.;

/// Opacity of interactively moved tiles targeting the scrolling layout.
const INTERACTIVE_MOVE_ALPHA: f64 = 0.75;

/// Longest side a dragged window preview shrinks to in the overview, logical px
/// — gnome-shell's `WINDOW_DND_SIZE` (`windowPreview.js:14`), handed to
/// `DND.makeDraggable` as `dragActorMaxSize` (`:108`).
const WINDOW_DND_SIZE: f64 = 256.;

/// How long that shrink takes — `SCALE_ANIMATION_TIME` (`dnd.js:11`),
/// EASE_OUT_QUAD. A fixed gnome-shell duration, not a configurable one; only
/// whether animations run at all is inherited.
const DND_SCALE_ANIMATION_TIME_MS: u32 = 250;

/// Amount of touchpad movement to toggle the overview.
const OVERVIEW_GESTURE_MOVEMENT: f64 = 300.;

const OVERVIEW_GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

/// Size-relative units.
pub struct SizeFrac;

synoik_render_elements! {
    LayoutElementRenderElement => {
        Wayland = WaylandSurfaceRenderElement<VulkanRenderer>,
        SolidColor = SolidColorRenderElement,
        BackgroundEffect = BackgroundEffectElement,
    }
}

pub type LayoutElementRenderSnapshot = RenderSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizingMode {
    Normal,
    Maximized,
    Fullscreen,
}

pub trait LayoutElement {
    /// Type that can be used as a unique ID of this element.
    type Id: PartialEq + std::fmt::Debug + Clone;

    /// Unique ID of this element.
    fn id(&self) -> &Self::Id;

    /// Updates the config for the element.
    fn update_config(&mut self, blur_config: synoik_config::Blur) {
        let _ = blur_config;
    }

    /// Visual size of the element.
    ///
    /// This is what the user would consider the size, i.e. excluding CSD shadows and whatnot.
    /// Corresponds to the Wayland window geometry size.
    fn size(&self) -> Size<i32, Logical>;

    /// Returns the location of the element's buffer relative to the element's visual geometry.
    ///
    /// I.e. if the element has CSD shadows, its buffer location will have negative coordinates.
    fn buf_loc(&self) -> Point<i32, Logical>;

    /// Size of everything the client committed, i.e. [`size`](Self::size) plus whatever the
    /// visual geometry crops off — CSD shadow margins, most of the time.
    ///
    /// The pair (`buf_loc`, `buf_size`) is the buffer rectangle; `size` is the part of it the user
    /// would point at.
    fn buf_size(&self) -> Size<i32, Logical>;

    /// Checks whether a point is in the element's input region.
    ///
    /// The point is relative to the element's visual geometry.
    fn is_in_input_region(&self, point: Point<f64, Logical>) -> bool;

    /// Renders the element at the given visual location.
    ///
    /// The element should be rendered in such a way that its visual geometry ends up at the given
    /// location.
    fn render(
        &self,
        mut ctx: RenderCtx,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayoutElementRenderElement),
    ) {
        self.render_popups(ctx.r(), location, scale, alpha, xray_pos, push);
        self.render_normal(ctx.r(), location, scale, alpha, xray_pos, push);
    }

    /// Renders the non-popup parts of the element.
    fn render_normal(
        &self,
        ctx: RenderCtx,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayoutElementRenderElement),
    ) {
        let _ = (ctx, location, scale, alpha, xray_pos, push);
    }

    /// Renders the popups of the element.
    fn render_popups(
        &self,
        ctx: RenderCtx,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
        alpha: f32,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(LayoutElementRenderElement),
    ) {
        let _ = (ctx, location, scale, alpha, xray_pos, push);
    }

    /// Renders the background effect behind the main surface of the element.
    #[allow(clippy::too_many_arguments)]
    fn render_background_effect(
        &self,
        _ctx: RenderCtx,
        _geometry: Rectangle<f64, Logical>,
        _scale: f64,
        _surface_anim_scale: Scale<f64>,
        _radius: CornerRadius,
        _xray_pos: XrayPos,
        _push: &mut dyn FnMut(BackgroundEffectElement),
    ) {
    }

    /// Requests the element to change its size.
    ///
    /// The size request is stored and will be continuously sent to the element on any further
    /// state changes.
    fn request_size(
        &mut self,
        size: Size<i32, Logical>,
        mode: SizingMode,
        animate: bool,
        transaction: Option<Transaction>,
    );

    /// Requests the element to change size once, clearing the request afterwards.
    fn request_size_once(&mut self, size: Size<i32, Logical>, animate: bool) {
        self.request_size(size, SizingMode::Normal, animate, None);
    }

    fn min_size(&self) -> Size<i32, Logical>;
    fn max_size(&self) -> Size<i32, Logical>;
    fn is_wl_surface(&self, wl_surface: &WlSurface) -> bool;
    fn has_ssd(&self) -> bool;
    fn set_preferred_scale_transform(&self, scale: output::Scale, transform: Transform);
    fn output_enter(&self, output: &Output);
    fn output_leave(&self, output: &Output);
    fn set_offscreen_data(&self, data: Option<OffscreenData>);
    fn set_activated(&mut self, active: bool);
    fn set_active_in_column(&mut self, active: bool);
    fn set_floating(&mut self, floating: bool);
    fn set_bounds(&self, bounds: Size<i32, Logical>);
    fn is_ignoring_opacity_window_rule(&self) -> bool;

    fn is_urgent(&self) -> bool;

    fn configure_intent(&self) -> ConfigureIntent;
    fn send_pending_configure(&mut self);

    /// The element's current sizing mode.
    ///
    /// This will *not* switch immediately after a [`LayoutElement::request_size()`] call.
    fn sizing_mode(&self) -> SizingMode;

    /// The sizing mode that we're requesting the element to assume.
    ///
    /// This *will* switch immediately after a [`LayoutElement::request_size()`] call.
    fn pending_sizing_mode(&self) -> SizingMode;

    /// Size previously requested through [`LayoutElement::request_size()`].
    fn requested_size(&self) -> Option<Size<i32, Logical>>;

    /// Non-fullscreen size that we expect this window has or will shortly have.
    ///
    /// This can be different from [`requested_size()`](LayoutElement::requested_size()). For
    /// example, for floating windows this will generally return the current window size, rather
    /// than the last size that we requested, since we want floating windows to be able to change
    /// size freely. But not always: if we just requested a floating window to resize and it hasn't
    /// responded to it yet, this will return the newly requested size.
    ///
    /// This function should never return a 0 size component. `None` means there's no known
    /// expected size (for example, the window is fullscreen).
    ///
    /// The default impl is for testing only, it will not preserve the window's own size changes.
    fn expected_size(&self) -> Option<Size<i32, Logical>> {
        if self.sizing_mode().is_fullscreen() {
            return None;
        }

        let mut requested = self.requested_size().unwrap_or_default();
        let current = self.size();
        if requested.w == 0 {
            requested.w = current.w;
        }
        if requested.h == 0 {
            requested.h = current.h;
        }
        Some(requested)
    }

    fn is_windowed_fullscreen(&self) -> bool {
        false
    }
    fn is_pending_windowed_fullscreen(&self) -> bool {
        false
    }
    fn request_windowed_fullscreen(&mut self, value: bool) {
        let _ = value;
    }

    /// The effective geometry corner radius for this element.
    ///
    /// Returns zero when the element is in windowed fullscreen, since fullscreen windows have
    /// square corners.
    ///
    /// This method only handles windowed fullscreen and not maximized/real fullscreen. This is
    /// because windowed fullscreen is handled by the element itself, whereas other sizing modes
    /// are handled externally by the Tile, so the corner radius changes for those modes is also
    /// handled externally.
    fn geometry_corner_radius(&self) -> CornerRadius {
        if self.is_windowed_fullscreen() {
            return CornerRadius::default();
        }
        self.rules().geometry_corner_radius.unwrap_or_default()
    }

    fn is_child_of(&self, parent: &Self) -> bool;

    /// Whether this window has a transient parent at all.
    ///
    /// Stands in for mutter's dialog window *types* when placing: xdg-shell has
    /// no types, so `rectangle_overlaps_some_window`'s "a dialog is not an
    /// obstacle" rule (place.c:503-548) keys off the parent instead. The
    /// approximation is one-sided — mutter still treats a `UTILITY` window as an
    /// obstacle, and a GTK utility window with a parent looks the same as a
    /// dialog from here.
    fn is_transient(&self) -> bool;

    /// Which half of the work area this window is edge-tiled to (GNOME
    /// Super+Left/Right), if any.
    fn edge_tiled_side(&self) -> Option<TileSide> {
        None
    }

    /// Marks the window as edge-tiled and updates its xdg tiled states.
    fn set_edge_tiled(&mut self, _side: Option<TileSide>) {}

    fn rules(&self) -> &ResolvedWindowRules;

    /// Runs periodic clean-up tasks.
    fn refresh(&self);

    fn take_animation_snapshot(&mut self) -> Option<LayoutElementRenderSnapshot>;

    fn set_interactive_resize(&mut self, data: Option<InteractiveResizeData>);
    fn cancel_interactive_resize(&mut self);
    fn interactive_resize_data(&self) -> Option<InteractiveResizeData>;

    fn on_commit(&mut self, serial: Serial);
}

#[derive(Debug)]
pub struct Layout<W: LayoutElement> {
    /// Monitors and workspaes in the layout.
    monitor_set: MonitorSet<W>,
    /// Whether the layout should draw as active.
    ///
    /// This normally indicates that the layout has keyboard focus, but not always. E.g. when the
    /// screenshot UI is open, it keeps the layout drawing as active.
    is_active: bool,
    /// Map from monitor name to id of its last active workspace.
    ///
    /// This data is stored upon monitor removal and is used to restore the active workspace when
    /// the monitor is reconnected.
    ///
    /// The workspace id does not necessarily point to a valid workspace. If it doesn't, then it is
    /// simply ignored.
    last_active_workspace_id: HashMap<String, WorkspaceId>,
    /// Ongoing interactive move.
    interactive_move: Option<InteractiveMoveState<W>>,
    /// Ongoing drag-and-drop operation.
    dnd: Option<DndData<W>>,
    /// Clock for driving animations.
    clock: Clock,
    /// Time that we last updated render elements for.
    update_render_elements_time: Duration,
    /// Whether the overview is open.
    ///
    /// This is a boolean flag that controls things like where input goes to. The actual animation
    /// is controlled by overview_progress.
    overview_open: bool,
    /// Whether the overview's app grid (show-apps state) is showing. Only
    /// meaningful while [`Self::overview_open`]; the per-monitor ease drives the
    /// WINDOW_PICKER↔APP_GRID box interpolation.
    app_grid_open: bool,
    /// The overview zoom progress.
    overview_progress: Option<OverviewProgress>,
    /// `org.gnome.mutter edge-tiling`: whether dragging a window to a screen
    /// edge tiles/maximizes it (GNOME windowing mode only). Pushed in from the
    /// GSettings model.
    gnome_edge_tiling: bool,
    /// Configurable properties of the layout.
    options: Rc<Options>,
}

#[derive(Debug)]
enum MonitorSet<W: LayoutElement> {
    /// At least one output is connected.
    Normal {
        /// Connected monitors.
        monitors: Vec<Monitor<W>>,
        /// Index of the primary monitor.
        primary_idx: usize,
        /// Index of the active monitor.
        active_monitor_idx: usize,
    },
    /// No outputs are connected, and these are the workspaces.
    NoOutputs {
        /// The workspaces.
        workspaces: Vec<Workspace<W>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub layout: synoik_config::Layout,
    pub animations: synoik_config::Animations,
    pub gestures: synoik_config::Gestures,
    pub overview: synoik_config::Overview,
    pub blur: synoik_config::Blur,
    /// `org.gnome.mutter center-new-windows`: place new windows in the middle
    /// of the work area rather than searching for a free spot.
    ///
    /// Owned by GSettings, not by the config — [`Layout::update_config`]
    /// carries it across a config reload, and
    /// [`Layout::set_gnome_center_new_windows`] is the only writer.
    pub gnome_center_new_windows: bool,
    /// `org.gnome.mutter auto-maximize`: whether a window covering most of the
    /// work area opens maximized. GSettings-owned like
    /// `gnome_center_new_windows`.
    pub gnome_auto_maximize: bool,
    // Debug flags.
    pub disable_resize_throttling: bool,
    pub disable_transactions: bool,
    pub deactivate_unfocused_windows: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            layout: Default::default(),
            animations: Default::default(),
            gestures: Default::default(),
            overview: Default::default(),
            blur: Default::default(),
            // GNOME's own schema defaults (centering since mutter 48,
            // 9fe83c736c). Spelled out here rather than derived so no `Options`
            // can be built with a GNOME behavior silently off.
            gnome_center_new_windows: true,
            gnome_auto_maximize: true,
            disable_resize_throttling: false,
            disable_transactions: false,
            deactivate_unfocused_windows: false,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum InteractiveMoveState<W: LayoutElement> {
    /// Initial rubberbanding; the window remains in the layout.
    Starting {
        /// The window we're moving.
        window_id: W::Id,
        /// Current pointer delta from the starting location.
        pointer_delta: Point<f64, Logical>,
        /// Pointer location within the visual window geometry as ratio from geometry size.
        ///
        /// This helps the pointer remain inside the window as it resizes.
        pointer_ratio_within_window: (f64, f64),
    },
    /// Moving; the window is no longer in the layout.
    Moving(InteractiveMoveData<W>),
}

#[derive(Debug)]
struct InteractiveMoveData<W: LayoutElement> {
    /// The window being moved.
    pub(self) tile: Tile<W>,
    /// Output where the window is currently located/rendered.
    pub(self) output: Output,
    /// Current pointer position within output.
    pub(self) pointer_pos_within_output: Point<f64, Logical>,
    /// Window column width.
    pub(self) width: ColumnWidth,
    /// Whether the window column was full-width.
    pub(self) is_full_width: bool,
    /// Whether the window targets the floating layout.
    pub(self) is_floating: bool,
    /// Pointer location within the visual window geometry as ratio from geometry size.
    ///
    /// This helps the pointer remain inside the window as it resizes.
    pub(self) pointer_ratio_within_window: (f64, f64),
    /// Config overrides for the output where the window is currently located.
    ///
    /// Cached here to be accessible while an output is removed.
    pub(self) output_config: Option<synoik_config::LayoutPart>,
    /// Config overrides for the workspace where the window is currently located.
    ///
    /// To avoid sudden window changes when starting an interactive move, it will remember the
    /// config overrides for the workspace where the move originated from. As soon as the window
    /// moves over some different workspace though, this override will reset.
    pub(self) workspace_config: Option<(WorkspaceId, synoik_config::LayoutPart)>,
    /// On-screen size of the window's picker preview when it was picked up
    /// in the GNOME overview.
    ///
    /// The dragged tile keeps rendering at this footprint: gnome-shell drags
    /// the preview, never resizing the real window.
    pub(self) expose_pickup_size: Option<Size<f64, Logical>>,
    /// Progress of the drag-actor shrink: gnome-shell eases the picked-up
    /// preview down to fit `WINDOW_DND_SIZE` on its longest side over
    /// `SCALE_ANIMATION_TIME` (`dnd.js:261-288`), so a drag across the row
    /// carries something small enough to see the target under.
    pub(self) expose_dnd_shrink: Option<Animation>,
}

#[derive(Debug)]
pub struct DndData<W: LayoutElement> {
    /// Output where the pointer is currently located.
    output: Output,
    /// Current pointer position within output.
    pointer_pos_within_output: Point<f64, Logical>,
    /// Ongoing DnD hold to activate something.
    hold: Option<DndHold<W>>,
}

#[derive(Debug)]
struct DndHold<W: LayoutElement> {
    /// Time when we started holding on the target.
    start_time: Duration,
    target: DndHoldTarget<W::Id>,
}

#[derive(Debug, PartialEq, Eq)]
enum DndHoldTarget<WindowId> {
    Window(WindowId),
    Workspace(WorkspaceId),
}

/// Where a switcher preview found a monitor, so the session can put it back — see
/// [`Layout::preview_workspace_of`].
///
/// Carries the origin workspace twice, by index *and* by id, because the two answer different
/// questions: the index is where to slide back to, and the id is what the "previous workspace"
/// bookmark should say afterwards. An index alone would go stale if the strip grew under the
/// preview; an id alone cannot be handed to `switch_workspace`.
#[derive(Debug, Clone)]
pub struct WorkspacePreviewOrigin {
    output: Output,
    idx: usize,
    id: WorkspaceId,
    previous: Option<WorkspaceId>,
}

impl WorkspacePreviewOrigin {
    pub fn output(&self) -> &Output {
        &self.output
    }
}

#[derive(Debug, Clone, Copy)]
pub struct InteractiveResizeData {
    pub(self) edges: ResizeEdge,
}

#[derive(Debug, Clone, Copy)]
pub enum ConfigureIntent {
    /// A configure is not needed (no changes to server pending state).
    NotNeeded,
    /// A configure is throttled (due to resizing too fast for example).
    Throttled,
    /// Can send the configure if it isn't throttled externally (only size changed).
    CanSend,
    /// Should send the configure regardless of external throttling (something other than size
    /// changed).
    ShouldSend,
}

/// Tile that was just removed from the layout.
pub struct RemovedTile<W: LayoutElement> {
    tile: Tile<W>,
    /// Width of the column the tile was in.
    width: ColumnWidth,
    /// Whether the column the tile was in was full-width.
    is_full_width: bool,
    /// Whether the tile was floating.
    is_floating: bool,
}

/// Whether to activate a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ActivateWindow {
    /// Activate unconditionally.
    Yes,
    /// Activate based on heuristics.
    #[default]
    Smart,
    /// Do not activate.
    No,
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum AddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// On this output.
    Output(&'a Output),
    /// On this workspace.
    Workspace(WorkspaceId),
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

/// Type of the window hit from `window_under()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HitType {
    /// The hit is within a window's input region and can be used for sending events to it.
    Input {
        /// Position of the window's buffer.
        win_pos: Point<f64, Logical>,
    },
    /// The hit can activate a window, but it is not in the input region so cannot send events.
    ///
    /// For example, this could be clicking on a tile border outside the window.
    Activate {
        /// Whether the hit was on the tab indicator.
        is_tab_indicator: bool,
    },
}

#[derive(Debug)]
enum OverviewProgress {
    Animation(Animation),
    Gesture(OverviewGesture),
    Open,
}

#[derive(Debug)]
struct OverviewGesture {
    tracker: SwipeTracker,
    /// Start point.
    start: f64,
    /// Current progress.
    value: f64,
}

/// A window's persistable layout state, from [`Layout::session_snapshot`].
#[derive(Debug)]
pub struct SessionSnapshot<'a> {
    /// The output the window is on, or `None` while there are no outputs at all.
    pub output: Option<&'a Output>,

    /// Index within `output`'s workspaces, not a [`WorkspaceId`]: ids are runtime-only and
    /// meaningless across restarts, which is also why mutter persists an index.
    pub workspace_idx: usize,

    pub sizing_mode: SizingMode,

    /// The rect the window would take if floating, **output-local**. `None` when it has never
    /// floated, so there is nothing remembered to restore it to.
    pub floating_rect: Option<Rectangle<f64, Logical>>,
}

impl SizingMode {
    #[must_use]
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::Normal)
    }

    #[must_use]
    pub fn is_fullscreen(&self) -> bool {
        matches!(self, Self::Fullscreen)
    }

    #[must_use]
    pub fn is_maximized(&self) -> bool {
        matches!(self, Self::Maximized)
    }
}

impl<W: LayoutElement> InteractiveMoveState<W> {
    fn moving(&self) -> Option<&InteractiveMoveData<W>> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_),
            _ => None,
        }
    }

    fn moving_mut(&mut self) -> Option<&mut InteractiveMoveData<W>> {
        match self {
            InteractiveMoveState::Moving(move_) => Some(move_),
            _ => None,
        }
    }
}

impl<W: LayoutElement> InteractiveMoveData<W> {
    /// Extra render scale that fits the dragged tile into the picker-preview
    /// footprint it had when picked up in the GNOME overview, shrunk toward
    /// `WINDOW_DND_SIZE` as the drag gets going.
    fn expose_extra_scale(&self, zoom: f64) -> f64 {
        let Some(pickup) = self.expose_pickup_size else {
            return 1.;
        };
        let size = self.tile.tile_size();
        if size.w <= 0. || size.h <= 0. {
            return 1.;
        }
        let fit = f64::min(pickup.w / (size.w * zoom), pickup.h / (size.h * zoom));
        fit * self.expose_dnd_shrink_factor(pickup)
    }

    /// How far the drag actor has shrunk toward [`WINDOW_DND_SIZE`]: 1 at the
    /// moment of pickup, easing to `WINDOW_DND_SIZE / longest side` (never up —
    /// a preview already smaller than that is left alone, `dnd.js:262-264`).
    fn expose_dnd_shrink_factor(&self, pickup: Size<f64, Logical>) -> f64 {
        let longest = f64::max(pickup.w, pickup.h);
        if longest <= WINDOW_DND_SIZE {
            return 1.;
        }
        let target = WINDOW_DND_SIZE / longest;
        let progress = self
            .expose_dnd_shrink
            .as_ref()
            .map_or(0., |anim| anim.clamped_value().clamp(0., 1.));
        1. + (target - 1.) * progress
    }

    fn tile_render_location(&self, zoom: f64) -> Point<f64, Logical> {
        let scale = Scale::from(self.output.current_scale().fractional_scale());
        let window_size = self.tile.window_size();
        let pointer_offset_within_window = Point::from((
            window_size.w * self.pointer_ratio_within_window.0,
            window_size.h * self.pointer_ratio_within_window.1,
        ));
        let render_scale = zoom * self.expose_extra_scale(zoom);
        let pos = self.pointer_pos_within_output
            - (pointer_offset_within_window + self.tile.window_loc() - self.tile.render_offset())
                .upscale(render_scale);
        // Round to physical pixels.
        pos.to_physical_precise_round(scale).to_logical(scale)
    }
}

impl ActivateWindow {
    pub fn map_smart(self, f: impl FnOnce() -> bool) -> bool {
        match self {
            ActivateWindow::Yes => true,
            ActivateWindow::Smart => f(),
            ActivateWindow::No => false,
        }
    }
}

impl HitType {
    pub fn offset_win_pos(mut self, offset: Point<f64, Logical>) -> Self {
        match &mut self {
            HitType::Input { win_pos } => *win_pos += offset,
            HitType::Activate { .. } => (),
        }
        self
    }

    pub fn hit_tile<W: LayoutElement>(
        tile: &Tile<W>,
        tile_pos: Point<f64, Logical>,
        point: Point<f64, Logical>,
    ) -> Option<(&W, Self)> {
        let pos_within_tile = point - tile_pos;
        tile.hit(pos_within_tile)
            .map(|hit| (tile.window(), hit.offset_win_pos(tile_pos)))
    }

    pub fn to_activate(self) -> Self {
        match self {
            HitType::Input { .. } => HitType::Activate {
                is_tab_indicator: false,
            },
            HitType::Activate { .. } => self,
        }
    }
}

impl Options {
    fn from_config(config: &Config) -> Self {
        Self {
            layout: config.layout.clone(),
            animations: config.animations.clone(),
            gestures: config.gestures,
            overview: config.overview,
            blur: config.blur,
            // GSettings-owned; `Layout::update_config` carries the live values
            // over these.
            gnome_center_new_windows: true,
            gnome_auto_maximize: true,
            disable_resize_throttling: config.debug.disable_resize_throttling,
            disable_transactions: config.debug.disable_transactions,
            deactivate_unfocused_windows: config.debug.deactivate_unfocused_windows,
        }
    }

    fn with_merged_layout(mut self, part: Option<&synoik_config::LayoutPart>) -> Self {
        if let Some(part) = part {
            self.layout.merge_with(part);
        }
        self
    }

    fn adjusted_for_scale(mut self, scale: f64) -> Self {
        self.layout.gaps = round_logical_in_physical_max1(scale, self.layout.gaps);
        self
    }
}

impl OverviewProgress {
    fn value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.value(),
            OverviewProgress::Gesture(gesture) => gesture.value,
            OverviewProgress::Open => 1.,
        }
    }

    fn is_animation(&self) -> bool {
        matches!(self, OverviewProgress::Animation(_))
    }
}

impl<W: LayoutElement> Layout<W> {
    pub fn new(clock: Clock, config: &Config) -> Self {
        Self::with_options_and_workspaces(clock, config, Options::from_config(config))
    }

    pub fn with_options(clock: Clock, options: Options) -> Self {
        Self {
            monitor_set: MonitorSet::NoOutputs { workspaces: vec![] },
            is_active: true,
            last_active_workspace_id: HashMap::new(),
            interactive_move: None,
            dnd: None,
            clock,
            update_render_elements_time: Duration::ZERO,
            overview_open: false,
            app_grid_open: false,
            overview_progress: None,
            gnome_edge_tiling: true,
            options: Rc::new(options),
        }
    }

    fn with_options_and_workspaces(clock: Clock, config: &Config, options: Options) -> Self {
        let opts = Rc::new(options);

        let workspaces = config
            .workspaces
            .iter()
            .map(|ws| {
                Workspace::new_with_config_no_outputs(Some(ws.clone()), clock.clone(), opts.clone())
            })
            .collect();

        Self {
            monitor_set: MonitorSet::NoOutputs { workspaces },
            is_active: true,
            last_active_workspace_id: HashMap::new(),
            interactive_move: None,
            dnd: None,
            clock,
            update_render_elements_time: Duration::ZERO,
            overview_open: false,
            app_grid_open: false,
            overview_progress: None,
            gnome_edge_tiling: true,
            options: opts,
        }
    }

    /// Pushes `org.gnome.mutter edge-tiling` in from the GSettings model.
    pub fn set_gnome_edge_tiling(&mut self, edge_tiling: bool) {
        self.gnome_edge_tiling = edge_tiling;
    }

    /// Pushes `org.gnome.mutter center-new-windows` in from the GSettings
    /// model. Unlike edge-tiling this one is read deep in the floating layout,
    /// so it rides [`Options`] down to every space instead of living here.
    pub fn set_gnome_center_new_windows(&mut self, center: bool) {
        self.update_gnome_option(|options| options.gnome_center_new_windows = center);
    }

    /// Pushes `org.gnome.mutter auto-maximize` in from the GSettings model.
    pub fn set_gnome_auto_maximize(&mut self, auto_maximize: bool) {
        self.update_gnome_option(|options| options.gnome_auto_maximize = auto_maximize);
    }

    /// Applies a GSettings-owned [`Options`] change, re-pushing the options to
    /// every space only when the value actually moved.
    fn update_gnome_option(&mut self, change: impl FnOnce(&mut Options)) {
        let mut options = (*self.options).clone();
        change(&mut options);
        if options != *self.options {
            self.update_options(options);
        }
    }

    pub fn add_output(&mut self, output: Output, layout_config: Option<LayoutPart>) {
        self.monitor_set = match mem::take(&mut self.monitor_set) {
            MonitorSet::Normal {
                mut monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let primary = &mut monitors[primary_idx];

                let mut took_from_primary = false;

                let mut workspaces = vec![];
                for i in (0..primary.workspaces.len()).rev() {
                    if primary.workspaces[i].original_output.matches(&output) {
                        let ws = primary.workspaces.remove(i);
                        took_from_primary = true;

                        // FIXME: this can be coded in a way that the workspace switch won't be
                        // affected if the removed workspace is invisible. But this is good enough
                        // for now.
                        primary.workspace_switch = None;

                        // The user could've closed a window while remaining on this workspace, on
                        // another monitor. However, we will add an empty workspace in the end
                        // instead.
                        if ws.has_windows_or_name() {
                            workspaces.push(ws);
                        }

                        if i <= primary.active_workspace_idx {
                            primary.active_workspace_idx =
                                primary.active_workspace_idx.saturating_sub(1);
                        }
                    }
                }

                // Whatever we took, the monitor we took it from still owes the workspace-list
                // invariants: a trailing empty, and gnome-shell's `MIN_NUM_WORKSPACES` of them.
                // This used to run only when a workspace switch had been stopped, which was
                // enough back when the only invariant was the trailing empty — the loop leaves
                // that one behind on its own. It is not enough for the minimum: handing a
                // two-workspace monitor's named workspace to the output that just came back
                // left it with one. Removing anything also clears the switch above, so the
                // `workspace_switch.is_none()` `clean_up_workspaces` asserts still holds.
                if took_from_primary {
                    primary.clean_up_workspaces();
                }

                workspaces.reverse();

                let ws_id_to_activate = self.last_active_workspace_id.remove(&output.name());

                let mut monitor = Monitor::new(
                    output,
                    workspaces,
                    ws_id_to_activate,
                    self.clock.clone(),
                    self.options.clone(),
                    layout_config,
                );
                monitor.overview_open = self.overview_open;
                monitor.set_overview_progress(self.overview_progress.as_ref());
                monitors.push(monitor);

                MonitorSet::Normal {
                    monitors,
                    primary_idx,
                    active_monitor_idx,
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws_id_to_activate = self.last_active_workspace_id.remove(&output.name());

                let mut monitor = Monitor::new(
                    output,
                    workspaces,
                    ws_id_to_activate,
                    self.clock.clone(),
                    self.options.clone(),
                    layout_config,
                );
                monitor.overview_open = self.overview_open;
                monitor.set_overview_progress(self.overview_progress.as_ref());

                MonitorSet::Normal {
                    monitors: vec![monitor],
                    primary_idx: 0,
                    active_monitor_idx: 0,
                }
            }
        }
    }

    pub fn remove_output(&mut self, output: &Output) {
        self.monitor_set = match mem::take(&mut self.monitor_set) {
            MonitorSet::Normal {
                mut monitors,
                mut primary_idx,
                mut active_monitor_idx,
            } => {
                let idx = monitors
                    .iter()
                    .position(|mon| &mon.output == output)
                    .expect("trying to remove non-existing output");
                let monitor = monitors.remove(idx);

                self.last_active_workspace_id.insert(
                    monitor.output_name().clone(),
                    monitor.workspaces[monitor.active_workspace_idx].id(),
                );

                let mut workspaces = monitor.into_workspaces();

                if monitors.is_empty() {
                    // Removed the last monitor.

                    for ws in &mut workspaces {
                        // Reset base options to layout ones.
                        ws.update_config(self.options.clone());
                    }

                    MonitorSet::NoOutputs { workspaces }
                } else {
                    if primary_idx >= idx {
                        // Update primary_idx to either still point at the same monitor, or at some
                        // other monitor if the primary has been removed.
                        primary_idx = primary_idx.saturating_sub(1);
                    }
                    if active_monitor_idx >= idx {
                        // Update active_monitor_idx to either still point at the same monitor, or
                        // at some other monitor if the active monitor has
                        // been removed.
                        active_monitor_idx = active_monitor_idx.saturating_sub(1);
                    }

                    let primary = &mut monitors[primary_idx];
                    primary.append_workspaces(workspaces);

                    MonitorSet::Normal {
                        monitors,
                        primary_idx,
                        active_monitor_idx,
                    }
                }
            }
            MonitorSet::NoOutputs { .. } => {
                panic!("tried to remove output when there were already none")
            }
        }
    }

    pub fn add_column_by_idx(
        &mut self,
        monitor_idx: usize,
        workspace_idx: usize,
        column: Column<W>,
        activate: bool,
    ) {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            panic!()
        };

        monitors[monitor_idx].add_column(workspace_idx, column, activate);

        if activate {
            *active_monitor_idx = monitor_idx;
        }
    }

    /// Adds a new window to the layout.
    ///
    /// Returns an output that the window was added to, if there were any outputs.
    #[allow(clippy::too_many_arguments)]
    pub fn add_window(
        &mut self,
        window: W,
        target: AddWindowTarget<W>,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_full_width: bool,
        is_floating: bool,
        activate: ActivateWindow,
    ) -> Option<&Output> {
        let scrolling_height = height.map(SizeChange::from);
        let id = window.id().clone();

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon_idx, target) = match target {
                    AddWindowTarget::Auto => (*active_monitor_idx, MonitorAddWindowTarget::Auto),
                    AddWindowTarget::Output(output) => {
                        let mon_idx = monitors
                            .iter()
                            .position(|mon| mon.output == *output)
                            .unwrap();

                        (mon_idx, MonitorAddWindowTarget::Auto)
                    }
                    AddWindowTarget::Workspace(ws_id) => {
                        let mon_idx = monitors
                            .iter()
                            .position(|mon| mon.workspaces.iter().any(|ws| ws.id() == ws_id))
                            .unwrap();

                        (
                            mon_idx,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: None,
                            },
                        )
                    }
                    AddWindowTarget::NextTo(next_to) => {
                        if let Some(output) = self
                            .interactive_move
                            .as_ref()
                            .and_then(|move_| {
                                if let InteractiveMoveState::Moving(move_) = move_ {
                                    Some(move_)
                                } else {
                                    None
                                }
                            })
                            .filter(|move_| next_to == move_.tile.window().id())
                            .map(|move_| move_.output.clone())
                        {
                            // The next_to window is being interactively moved.
                            let mon_idx = monitors
                                .iter()
                                .position(|mon| mon.output == output)
                                .unwrap_or(*active_monitor_idx);

                            (mon_idx, MonitorAddWindowTarget::Auto)
                        } else {
                            let mon_idx = monitors
                                .iter()
                                .position(|mon| {
                                    mon.workspaces.iter().any(|ws| ws.has_window(next_to))
                                })
                                .unwrap();
                            (mon_idx, MonitorAddWindowTarget::NextTo(next_to))
                        }
                    }
                };
                let mon = &mut monitors[mon_idx];

                let (ws_idx, _) = mon.resolve_add_window_target(target);
                let ws = &mon.workspaces[ws_idx];
                let scrolling_width = ws.resolve_scrolling_width(&window, width);

                mon.add_window(
                    window,
                    target,
                    activate,
                    scrolling_width,
                    is_full_width,
                    is_floating,
                );

                if activate.map_smart(|| false) {
                    *active_monitor_idx = mon_idx;
                }

                // Set the default height for scrolling windows.
                if !is_floating {
                    if let Some(change) = scrolling_height {
                        let ws = mon
                            .workspaces
                            .iter_mut()
                            .find(|ws| ws.has_window(&id))
                            .unwrap();
                        ws.set_window_height(Some(&id), change);
                    }
                }

                Some(&mon.output)
            }
            MonitorSet::NoOutputs { workspaces } => {
                let (ws_idx, target) = match target {
                    AddWindowTarget::Auto => {
                        if workspaces.is_empty() {
                            workspaces.push(Workspace::new_no_outputs(
                                self.clock.clone(),
                                self.options.clone(),
                            ));
                        }

                        (0, WorkspaceAddWindowTarget::Auto)
                    }
                    AddWindowTarget::Output(_) => panic!(),
                    AddWindowTarget::Workspace(ws_id) => {
                        let ws_idx = workspaces.iter().position(|ws| ws.id() == ws_id).unwrap();
                        (ws_idx, WorkspaceAddWindowTarget::Auto)
                    }
                    AddWindowTarget::NextTo(next_to) => {
                        if self
                            .interactive_move
                            .as_ref()
                            .and_then(|move_| {
                                if let InteractiveMoveState::Moving(move_) = move_ {
                                    Some(move_)
                                } else {
                                    None
                                }
                            })
                            .filter(|move_| next_to == move_.tile.window().id())
                            .is_some()
                        {
                            // The next_to window is being interactively moved. If there are no
                            // other windows, we may have no workspaces at all.
                            if workspaces.is_empty() {
                                workspaces.push(Workspace::new_no_outputs(
                                    self.clock.clone(),
                                    self.options.clone(),
                                ));
                            }

                            (0, WorkspaceAddWindowTarget::Auto)
                        } else {
                            let ws_idx = workspaces
                                .iter()
                                .position(|ws| ws.has_window(next_to))
                                .unwrap();
                            (ws_idx, WorkspaceAddWindowTarget::NextTo(next_to))
                        }
                    }
                };
                let ws = &mut workspaces[ws_idx];

                let scrolling_width = ws.resolve_scrolling_width(&window, width);

                let tile = ws.make_tile(window);
                ws.add_tile(
                    tile,
                    target,
                    activate,
                    scrolling_width,
                    is_full_width,
                    is_floating,
                );

                // Set the default height for scrolling windows.
                if !is_floating {
                    if let Some(change) = scrolling_height {
                        ws.set_window_height(Some(&id), change);
                    }
                }

                None
            }
        }
    }

    pub fn remove_window(
        &mut self,
        window: &W::Id,
        transaction: Transaction,
    ) -> Option<RemovedTile<W>> {
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting { window_id, .. } => {
                    if window_id == window {
                        self.interactive_move_end(window);
                    }
                }
                InteractiveMoveState::Moving(move_) => {
                    if move_.tile.window().id() == window {
                        let Some(InteractiveMoveState::Moving(move_)) =
                            self.interactive_move.take()
                        else {
                            unreachable!()
                        };

                        for mon in self.monitors_mut() {
                            mon.dnd_scroll_gesture_end();
                        }

                        // Unlock the view on the workspaces.
                        for ws in self.workspaces_mut() {
                            ws.dnd_scroll_gesture_end();
                            ws.unfreeze_expose();
                        }

                        return Some(RemovedTile {
                            tile: move_.tile,
                            width: move_.width,
                            is_full_width: move_.is_full_width,
                            is_floating: false,
                        });
                    }
                }
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in mon.workspaces.iter_mut() {
                        if ws.has_window(window) {
                            // Emptying a workspace no longer reaps it: it stays put and
                            // grows a close button in the overview instead (see
                            // `Monitor::clean_up_workspaces`).
                            return Some(ws.remove_tile(window, transaction));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for (idx, ws) in workspaces.iter_mut().enumerate() {
                    if ws.has_window(window) {
                        let removed = ws.remove_tile(window, transaction);

                        // Clean up empty workspaces.
                        if !ws.has_windows_or_name() {
                            workspaces.remove(idx);
                        }

                        return Some(removed);
                    }
                }
            }
        }

        None
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        for ws in self.workspaces_mut() {
            if ws.descendants_added(id) {
                return true;
            }
        }

        false
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                // Do this before calling update_window() so it can get up-to-date info.
                if let Some(serial) = serial {
                    move_.tile.window_mut().on_commit(serial);
                }

                move_.tile.update_window();
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.update_window(window, serial);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.update_window(window, serial);
                        return;
                    }
                }
            }
        }
    }

    pub fn find_workspace_by_id(&self, id: WorkspaceId) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some((index, workspace)) = mon
                        .workspaces
                        .iter()
                        .enumerate()
                        .find(|(_, w)| w.id() == id)
                    {
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) =
                    workspaces.iter().enumerate().find(|(_, w)| w.id() == id)
                {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    pub fn find_workspace_by_name(&self, workspace_name: &str) -> Option<(usize, &Workspace<W>)> {
        match &self.monitor_set {
            MonitorSet::Normal { ref monitors, .. } => {
                for mon in monitors {
                    if let Some((index, workspace)) =
                        mon.workspaces.iter().enumerate().find(|(_, w)| {
                            w.name
                                .as_ref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
                        })
                    {
                        return Some((index, workspace));
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                if let Some((index, workspace)) = workspaces.iter().enumerate().find(|(_, w)| {
                    w.name
                        .as_ref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
                }) {
                    return Some((index, workspace));
                }
            }
        }

        None
    }

    pub fn find_workspace_by_ref(
        &mut self,
        reference: WorkspaceReference,
    ) -> Option<&mut Workspace<W>> {
        if let WorkspaceReference::Index(index) = reference {
            self.active_monitor().and_then(|m| {
                let index = index.saturating_sub(1) as usize;
                m.workspaces.get_mut(index)
            })
        } else {
            self.workspaces_mut().find(|ws| match &reference {
                WorkspaceReference::Name(ref_name) => ws
                    .name
                    .as_ref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(ref_name)),
                WorkspaceReference::Id(id) => ws.id().get() == *id,
                WorkspaceReference::Index(_) => unreachable!(),
            })
        }
    }

    pub fn unname_workspace(&mut self, workspace_name: &str) {
        self.unname_workspace_by_ref(WorkspaceReference::Name(workspace_name.into()));
    }

    pub fn unname_workspace_by_ref(&mut self, reference: WorkspaceReference) {
        let id = self.find_workspace_by_ref(reference).map(|ws| ws.id());
        if let Some(id) = id {
            self.unname_workspace_by_id(id);
        }
    }

    pub fn unname_workspace_by_id(&mut self, id: WorkspaceId) {
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    if mon.unname_workspace(id) {
                        return;
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for (idx, ws) in workspaces.iter_mut().enumerate() {
                    if ws.id() == id {
                        ws.unname();

                        // Clean up empty workspaces.
                        if !ws.has_windows() {
                            workspaces.remove(idx);
                        }

                        return;
                    }
                }
            }
        }
    }

    pub fn find_window_and_output(&self, wl_surface: &WlSurface) -> Option<(&W, Option<&Output>)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().is_wl_surface(wl_surface) {
                return Some((move_.tile.window(), Some(&move_.output)));
            }
        }

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mon.workspaces {
                        if let Some(window) = ws.find_wl_surface(wl_surface) {
                            return Some((window, Some(&mon.output)));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    if let Some(window) = ws.find_wl_surface(wl_surface) {
                        return Some((window, None));
                    }
                }
            }
        }

        None
    }

    pub fn find_window_and_output_mut(
        &mut self,
        wl_surface: &WlSurface,
    ) -> Option<(&mut W, Option<&Output>)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().is_wl_surface(wl_surface) {
                return Some((move_.tile.window_mut(), Some(&move_.output)));
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if let Some(window) = ws.find_wl_surface_mut(wl_surface) {
                            return Some((window, Some(&mon.output)));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    if let Some(window) = ws.find_wl_surface_mut(wl_surface) {
                        return Some((window, None));
                    }
                }
            }
        }

        None
    }

    /// Computes the window-geometry-relative target rect for popup unconstraining.
    ///
    /// We will try to fit popups inside this rect.
    pub fn popup_target_rect(&self, window: &W::Id) -> Rectangle<f64, Logical> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                // Follow the scrolling layout logic and fit the popup horizontally within the
                // window geometry.
                let width = move_.tile.window_size().w;
                let height = output_size(&move_.output).h;
                let mut target = Rectangle::from_size(Size::from((width, height)));
                // FIXME: ideally this shouldn't include the tile render offset, but the code
                // duplication would be a bit annoying for this edge case.
                target.loc.y -= move_.tile_render_location(1.).y;
                target.loc.y -= move_.tile.window_loc().y;
                return target;
            }
        }

        self.workspaces()
            .find_map(|(_, _, ws)| ws.popup_target_rect(window))
            .unwrap()
    }

    pub fn update_output_size(&mut self, output: &Output) {
        let _span = tracy_client::span!("Layout::update_output_size");

        let Some(mon) = self.monitor_for_output_mut(output) else {
            error!("monitor missing in update_output_size()");
            return;
        };

        mon.update_output_size();
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return 0.;
            }
        }

        for mon in self.monitors() {
            for ws in &mon.workspaces {
                if ws.has_window(window) {
                    return ws.scroll_amount_to_activate(window);
                }
            }
        }

        0.
    }

    pub fn should_trigger_focus_follows_mouse_on(&self, window: &W::Id) -> bool {
        // During an animation, it's easy to trigger focus-follows-mouse on the previous workspace,
        // especially when clicking to switch workspace on a bar of some kind. This cancels the
        // workspace switch, which is annoying and not intended.
        //
        // This function allows focus-follows-mouse to trigger only on the animation target
        // workspace.
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return true;
            }
        }

        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            return true;
        };

        let (mon, ws_idx) = monitors
            .iter()
            .find_map(|mon| {
                mon.workspaces
                    .iter()
                    .position(|ws| ws.has_window(window))
                    .map(|ws_idx| (mon, ws_idx))
            })
            .unwrap();

        // During a gesture, focus-follows-mouse does not cause any unintended workspace switches.
        if let Some(WorkspaceSwitch::Gesture(_)) = mon.workspace_switch {
            return true;
        }

        ws_idx == mon.active_workspace_idx
    }

    /// Bring `under` to the front of `window`'s workspace, ready for `window` to be activated on
    /// top of them — the raising half of `shell_app_activate_window` (`shell-app.c:413-425`).
    ///
    /// Two rules come straight from there and are the whole content of this function. It raises
    /// **in reverse**, so the group arrives with its relative stacking intact rather than
    /// re-sorted; and it raises only what shares `window`'s workspace, so activating an app does
    /// not reshuffle its windows on desktops you are not looking at.
    pub fn raise_under(&mut self, window: &W::Id, under: &[W::Id]) {
        if under.is_empty() {
            return;
        }

        let Some(ws) = self.workspaces_mut().find(|ws| ws.has_window(window)) else {
            return;
        };
        for id in under.iter().rev() {
            if id != window {
                ws.raise_window_only(id);
            }
        }
    }

    pub fn activate_window(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };

        for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
            for (workspace_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                if ws.activate_window(window) {
                    *active_monitor_idx = monitor_idx;

                    // If currently in the middle of a vertical swipe between the target workspace
                    // and some other, don't switch the workspace.
                    match &mon.workspace_switch {
                        Some(WorkspaceSwitch::Gesture(gesture))
                            if gesture.current_idx.floor() == workspace_idx as f64
                                || gesture.current_idx.ceil() == workspace_idx as f64 => {}
                        _ => mon.switch_workspace(workspace_idx),
                    }

                    return;
                }
            }
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return;
        };

        for (monitor_idx, mon) in monitors.iter_mut().enumerate() {
            for (workspace_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                if ws.activate_window_without_raising(window) {
                    *active_monitor_idx = monitor_idx;

                    // If currently in the middle of a vertical swipe between the target workspace
                    // and some other, don't switch the workspace.
                    match &mon.workspace_switch {
                        Some(WorkspaceSwitch::Gesture(gesture))
                            if gesture.current_idx.floor() == workspace_idx as f64
                                || gesture.current_idx.ceil() == workspace_idx as f64 => {}
                        _ => mon.switch_workspace(workspace_idx),
                    }

                    return;
                }
            }
        }
    }

    pub fn active_output(&self) -> Option<&Output> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx].output)
    }

    pub fn active_workspace(&self) -> Option<&Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        let mon = &monitors[*active_monitor_idx];
        Some(&mon.workspaces[mon.active_workspace_idx])
    }

    pub fn active_workspace_mut(&mut self) -> Option<&mut Workspace<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        let mon = &mut monitors[*active_monitor_idx];
        Some(&mut mon.workspaces[mon.active_workspace_idx])
    }

    /// Every window on `output`'s **active** workspace with its output-local logical rect, front
    /// to back.
    ///
    /// The screenshot UI's window selector. GNOME filters the same three ways — not
    /// override-redirect, on the active workspace, on this monitor (`UIWindowSelector.capture`,
    /// `js/ui/screenshot.js:1063-1071`) — and feeds the rects to the same layout strategy the
    /// overview uses, which is [`expose::compute_slots`] here too.
    ///
    /// Distinct from [`Self::windows_for_output`], which spans *every* workspace on the monitor:
    /// a selector offering windows from a workspace you cannot see would be a menu of surprises.
    pub fn active_workspace_windows_for_output(
        &self,
        output: &Output,
    ) -> Vec<(&W, Rectangle<f64, Logical>)> {
        let Some(mon) = self.monitor_for_output(output) else {
            return Vec::new();
        };
        let ws = &mon.workspaces[mon.active_workspace_idx];
        ws.tiles_with_render_positions()
            .map(|(tile, pos, _)| (tile.window(), Rectangle::new(pos, tile.tile_size())))
            .collect()
    }

    pub fn windows_for_output(&self, output: &Output) -> impl Iterator<Item = &W> + '_ {
        let MonitorSet::Normal { monitors, .. } = &self.monitor_set else {
            panic!()
        };

        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .filter(|move_| move_.output == *output)
            .map(|move_| move_.tile.window())
            .into_iter();

        let mon = monitors.iter().find(|mon| &mon.output == output).unwrap();
        let mon_windows = mon.workspaces.iter().flat_map(|ws| ws.windows());

        moving_window.chain(mon_windows)
    }

    pub fn windows_for_output_mut(&mut self, output: &Output) -> impl Iterator<Item = &mut W> + '_ {
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            panic!()
        };

        let moving_window = self
            .interactive_move
            .as_mut()
            .and_then(|x| x.moving_mut())
            .filter(|move_| move_.output == *output)
            .map(|move_| move_.tile.window_mut())
            .into_iter();

        let mon = monitors
            .iter_mut()
            .find(|mon| &mon.output == output)
            .unwrap();
        let mon_windows = mon.workspaces.iter_mut().flat_map(|ws| ws.windows_mut());

        moving_window.chain(mon_windows)
    }

    pub fn with_windows(
        &self,
        mut f: impl FnMut(&W, Option<&Output>, Option<WorkspaceId>, WindowLayout),
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            // We don't fill any positions for interactively moved windows.
            let layout = move_.tile.ipc_layout_template();
            f(move_.tile.window(), Some(&move_.output), None, layout);
        }

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mon.workspaces {
                        for (tile, layout) in ws.tiles_with_ipc_layouts() {
                            f(tile.window(), Some(&mon.output), Some(ws.id()), layout);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    for (tile, layout) in ws.tiles_with_ipc_layouts() {
                        f(tile.window(), None, Some(ws.id()), layout);
                    }
                }
            }
        }
    }

    pub fn with_windows_mut(&mut self, mut f: impl FnMut(&mut W, Option<&Output>)) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            f(move_.tile.window_mut(), Some(&move_.output));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        for win in ws.windows_mut() {
                            f(win, Some(&mon.output));
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    for win in ws.windows_mut() {
                        f(win, None);
                    }
                }
            }
        }
    }

    fn active_monitor(&mut self) -> Option<&mut Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        Some(&mut monitors[*active_monitor_idx])
    }

    pub fn active_monitor_ref(&self) -> Option<&Monitor<W>> {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        Some(&monitors[*active_monitor_idx])
    }

    pub fn monitors(&self) -> impl Iterator<Item = &Monitor<W>> + '_ {
        let monitors = if let MonitorSet::Normal { monitors, .. } = &self.monitor_set {
            &monitors[..]
        } else {
            &[][..]
        };

        monitors.iter()
    }

    pub fn monitors_mut(&mut self) -> impl Iterator<Item = &mut Monitor<W>> + '_ {
        let monitors = if let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set {
            &mut monitors[..]
        } else {
            &mut [][..]
        };

        monitors.iter_mut()
    }

    pub fn monitor_for_output(&self, output: &Output) -> Option<&Monitor<W>> {
        self.monitors().find(|mon| &mon.output == output)
    }

    pub fn monitor_for_output_mut(&mut self, output: &Output) -> Option<&mut Monitor<W>> {
        self.monitors_mut().find(|mon| &mon.output == output)
    }

    /// Whether `id` is the active workspace on its *own* monitor.
    ///
    /// The question a mapping window has to answer before it is allowed to take focus: landing on
    /// another monitor's active workspace is ordinary, landing on a workspace that monitor is not
    /// showing means following it would move the user.
    pub fn workspace_is_active(&self, id: WorkspaceId) -> bool {
        self.workspaces().any(|(mon, idx, ws)| {
            ws.id() == id && mon.is_some_and(|mon| mon.active_workspace_idx() == idx)
        })
    }

    /// The workspace a restored window belongs on, growing the strip if the index is past the end.
    ///
    /// See [`Monitor::ensure_workspace_at`] for why restore grows rather than clamps.
    pub fn ensure_restore_workspace(&mut self, output: &Output, idx: usize) -> Option<WorkspaceId> {
        let mon = self.monitor_for_output_mut(output)?;
        Some(mon.ensure_workspace_at(idx).id())
    }

    pub fn monitor_for_workspace(&self, workspace_name: &str) -> Option<&Monitor<W>> {
        self.monitors().find(|monitor| {
            monitor.workspaces.iter().any(|ws| {
                ws.name
                    .as_ref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
            })
        })
    }

    pub fn outputs(&self) -> impl Iterator<Item = &Output> + '_ {
        self.monitors().map(|mon| &mon.output)
    }

    pub fn move_left(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_left();
    }

    pub fn move_right(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_right();
    }

    pub fn move_column_to_first(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_column_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_column_to_last();
    }

    pub fn move_column_left_or_to_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.move_left() {
                return false;
            }
        }

        self.move_column_to_output(output, None, true);
        true
    }

    pub fn move_column_right_or_to_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.move_right() {
                return false;
            }
        }

        self.move_column_to_output(output, None, true);
        true
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_column_to_index(index);
    }

    pub fn move_down(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_down();
    }

    pub fn move_up(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.move_up();
    }

    pub fn move_down_or_to_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_down_or_to_workspace_down();
    }

    pub fn move_up_or_to_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_up_or_to_workspace_up();
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.consume_or_expel_window_left(window);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.consume_or_expel_window_right(window);
    }

    pub fn focus_left(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_left();
    }

    pub fn focus_right(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_right();
    }

    pub fn focus_column_first(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_column_first();
    }

    pub fn focus_column_last(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_column_last();
    }

    pub fn focus_column_right_or_first(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_column_right_or_first();
    }

    pub fn focus_column_left_or_last(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_column_left_or_last();
    }

    pub fn focus_column(&mut self, index: usize) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_column(index);
    }

    pub fn focus_window_up_or_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_up() {
                return false;
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_window_down_or_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_down() {
                return false;
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_column_left_or_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_left() {
                return false;
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_column_right_or_output(&mut self, output: &Output) -> bool {
        if let Some(workspace) = self.active_workspace_mut() {
            if workspace.focus_right() {
                return false;
            }
        }

        self.focus_output(output);
        true
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_window_in_column(index);
    }

    pub fn focus_down(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_down();
    }

    pub fn focus_up(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_up();
    }

    pub fn focus_down_or_left(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_down_or_left();
    }

    pub fn focus_down_or_right(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_down_or_right();
    }

    pub fn focus_up_or_left(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_up_or_left();
    }

    pub fn focus_up_or_right(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_up_or_right();
    }

    pub fn focus_window_or_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_window_or_workspace_down();
    }

    pub fn focus_window_or_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.focus_window_or_workspace_up();
    }

    pub fn focus_window_top(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_window_top();
    }

    pub fn focus_window_bottom(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_window_bottom();
    }

    pub fn focus_window_down_or_top(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_window_down_or_top();
    }

    pub fn focus_window_up_or_bottom(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_window_up_or_bottom();
    }

    pub fn move_to_workspace_up(&mut self, focus: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_to_workspace_up(focus);
    }

    pub fn move_to_workspace_down(&mut self, focus: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_to_workspace_down(focus);
    }

    pub fn move_to_workspace(
        &mut self,
        window: Option<&W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let monitor = if let Some(window) = window {
            match &mut self.monitor_set {
                MonitorSet::Normal { monitors, .. } => monitors
                    .iter_mut()
                    .find(|mon| mon.has_window(window))
                    .unwrap(),
                MonitorSet::NoOutputs { .. } => {
                    return;
                }
            }
        } else {
            let Some(monitor) = self.active_monitor() else {
                return;
            };
            monitor
        };
        monitor.move_to_workspace(window, idx, activate);
    }

    pub fn move_column_to_workspace_up(&mut self, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace_up(activate);
    }

    pub fn move_column_to_workspace_down(&mut self, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace_down(activate);
    }

    pub fn move_column_to_workspace(&mut self, idx: usize, activate: bool) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_column_to_workspace(idx, activate);
    }

    pub fn switch_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_up();
    }

    pub fn switch_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_down();
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace(idx);
    }

    pub fn switch_workspace_auto_back_and_forth(&mut self, idx: usize) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_auto_back_and_forth(idx);
    }

    pub fn switch_workspace_previous(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.switch_workspace_previous();
    }

    pub fn consume_into_column(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.consume_into_column();
    }

    pub fn expel_from_column(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.expel_from_column();
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.swap_window_in_direction(direction);
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.toggle_column_tabbed_display();
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.set_column_display(display);
    }

    pub fn center_column(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.center_column();
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if id.is_none() || id == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(id) = id {
            Some(self.workspaces_mut().find(|ws| ws.has_window(id)).unwrap())
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.center_window(id);
    }

    pub fn center_visible_columns(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.center_visible_columns();
    }

    pub fn focus(&self) -> Option<&W> {
        self.focus_with_output().map(|(win, _out)| win)
    }

    pub fn focus_mut(&mut self) -> Option<&mut W> {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            return Some(move_.tile.window_mut());
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return None;
        };

        let mon = &mut monitors[*active_monitor_idx];
        mon.active_workspace().active_window_mut()
    }

    pub fn focus_with_output(&self) -> Option<(&W, &Output)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            return Some((move_.tile.window(), &move_.output));
        }

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return None;
        };

        let mon = &monitors[*active_monitor_idx];
        mon.active_window().map(|win| (win, &mon.output))
    }

    pub fn interactive_moved_window_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, HitType)> {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.output == *output {
                if self.overview_progress.is_some() {
                    let zoom = self.overview_zoom_for_output(output);
                    let tile_pos = move_.tile_render_location(zoom);
                    let pos_within_tile = (pos_within_output - tile_pos).downscale(zoom);
                    // During the overview animation, we cannot do input hits because we cannot
                    // really represent scaled windows properly.
                    let (win, hit) =
                        HitType::hit_tile(&move_.tile, Point::from((0., 0.)), pos_within_tile)?;
                    Some((win, hit.to_activate()))
                } else {
                    let tile_pos = move_.tile_render_location(1.);
                    HitType::hit_tile(&move_.tile, tile_pos, pos_within_output)
                }
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Returns the window under the cursor and the hit type.
    pub fn window_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&W, HitType)> {
        let mon = self.monitor_for_output(output)?;
        mon.window_under(pos_within_output)
    }

    pub fn resize_edges_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<ResizeEdge> {
        let mon = self.monitor_for_output(output)?;
        mon.resize_edges_under(pos_within_output)
    }

    pub fn workspace_under(
        &self,
        extended_bounds: bool,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        if self
            .interactive_moved_window_under(output, pos_within_output)
            .is_some()
        {
            return None;
        }

        let mon = self.monitor_for_output(output)?;
        if extended_bounds {
            mon.workspace_under(pos_within_output).map(|(ws, _)| ws)
        } else {
            mon.workspace_under_narrow(pos_within_output)
        }
    }

    pub fn thumbnail_workspace_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        let mon = self.monitor_for_output(output)?;
        mon.thumbnail_workspace_under(pos_within_output)
    }

    /// The workspace whose thumbnail close button is under the position.
    pub fn thumbnail_close_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<WorkspaceId> {
        let mon = self.monitor_for_output(output)?;
        mon.thumbnail_close_under(pos_within_output)
    }

    /// Closes an empty workspace from its thumbnail. Returns whether it went.
    pub fn close_workspace(&mut self, id: WorkspaceId) -> bool {
        for mon in self.monitors_mut() {
            if mon.close_workspace(id) {
                return true;
            }
        }
        false
    }

    /// The workspace zoom on an output. In GNOME mode the fully-zoomed-out
    /// size follows that monitor's overview chrome, so this is per-output
    /// rather than a layout-wide constant.
    pub fn overview_zoom_for_output(&self, output: &Output) -> f64 {
        let Some(mon) = self.monitor_for_output(output) else {
            return 1.;
        };
        mon.overview_zoom()
    }

    /// The workspace zoom on the output an interactive move is happening on
    /// (1 when nothing is being dragged).
    fn interactive_move_zoom(&self) -> f64 {
        let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move else {
            return 1.;
        };
        self.overview_zoom_for_output(&move_.output)
    }

    /// The allocated boxes of the overview chrome on an output
    /// (gnome-shell's `ControlsManagerLayout`).
    pub fn controls_layout_for_output(&self, output: &Output) -> Option<ControlsLayout> {
        Some(self.monitor_for_output(output)?.controls_layout())
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        use std::collections::HashSet;

        use approx::assert_abs_diff_eq;

        let zoom = self.interactive_move_zoom();

        let mut move_win_id = None;
        if let Some(state) = &self.interactive_move {
            match state {
                InteractiveMoveState::Starting {
                    window_id,
                    pointer_delta: _,
                    pointer_ratio_within_window: _,
                } => {
                    assert!(
                        self.has_window(window_id),
                        "interactive move must be on an existing window"
                    );
                    move_win_id = Some(window_id.clone());
                }
                InteractiveMoveState::Moving(move_) => {
                    assert_eq!(self.clock, move_.tile.clock);
                    // A GNOME overview drag carries the window state
                    // untouched, so the tile may still be maximized,
                    // fullscreen or edge-tiled mid-move.
                    if self.options.layout.windowing_mode != WindowingMode::Floating {
                        assert!(move_.tile.window().pending_sizing_mode().is_normal());
                    }

                    move_.tile.verify_invariants();

                    let scale = move_.output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options)
                        .with_merged_layout(move_.output_config.as_ref())
                        .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                        .adjusted_for_scale(scale);
                    assert_eq!(
                        &*move_.tile.options, &options,
                        "interactive moved tile options must be \
                         base options adjusted for output scale"
                    );

                    let tile_pos = move_.tile_render_location(zoom);
                    let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                    // Tile position must be rounded to physical pixels.
                    assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
                    assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

                    if let Some(alpha) = &move_.tile.alpha_animation {
                        if move_.is_floating {
                            assert_eq!(
                                alpha.anim.to(),
                                1.,
                                "interactively moved floating tile can animate alpha only to 1"
                            );

                            assert!(
                                !alpha.hold_after_done,
                                "interactively moved floating tile \
                                 cannot have held alpha animation"
                            );
                        } else {
                            assert_ne!(
                                alpha.anim.to(),
                                1.,
                                "interactively moved scrolling tile must animate alpha to not 1"
                            );

                            assert!(
                                alpha.hold_after_done,
                                "interactively moved scrolling tile \
                                 must have held alpha animation"
                            );
                        }
                    }
                }
            }
        }

        let mut seen_workspace_id = HashSet::new();
        let mut seen_workspace_name = Vec::<String>::new();

        let (monitors, &primary_idx, &active_monitor_idx) = match &self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => (monitors, primary_idx, active_monitor_idx),
            MonitorSet::NoOutputs { workspaces } => {
                for workspace in workspaces {
                    assert!(
                        workspace.has_windows_or_name(),
                        "with no outputs there cannot be empty unnamed workspaces"
                    );

                    assert_eq!(self.clock, workspace.clock);

                    assert_eq!(
                        workspace.base_options, self.options,
                        "workspace base options must be synchronized with layout"
                    );

                    assert!(
                        seen_workspace_id.insert(workspace.id()),
                        "workspace id must be unique"
                    );

                    if let Some(name) = &workspace.name {
                        assert!(
                            !seen_workspace_name
                                .iter()
                                .any(|n| n.eq_ignore_ascii_case(name)),
                            "workspace name must be unique"
                        );
                        seen_workspace_name.push(name.clone());
                    }

                    workspace.verify_invariants(move_win_id.as_ref());
                }

                return;
            }
        };

        assert!(primary_idx < monitors.len());
        assert!(active_monitor_idx < monitors.len());

        let mut saw_view_offset_gesture = false;

        for (idx, monitor) in monitors.iter().enumerate() {
            assert_eq!(self.clock, monitor.clock);
            assert_eq!(
                monitor.base_options, self.options,
                "monitor base options must be synchronized with layout"
            );

            assert_eq!(self.overview_open, monitor.overview_open);
            assert_eq!(
                self.overview_progress.as_ref().map(|p| p.value()),
                monitor.overview_progress_value()
            );

            monitor.verify_invariants();

            if idx == primary_idx {
                for ws in &monitor.workspaces {
                    if ws.original_output.matches(&monitor.output) {
                        // This is the primary monitor's own workspace.
                        continue;
                    }

                    let own_monitor_exists = monitors
                        .iter()
                        .any(|m| ws.original_output.matches(&m.output));
                    assert!(
                        !own_monitor_exists,
                        "primary monitor cannot have workspaces for which their own monitor exists"
                    );
                }
            } else {
                assert!(
                    monitor
                        .workspaces
                        .iter()
                        .any(|workspace| workspace.original_output.matches(&monitor.output)),
                    "secondary monitor must not have any non-own workspaces"
                );
            }

            // FIXME: verify that primary doesn't have any workspaces for which their own monitor
            // exists.

            for workspace in &monitor.workspaces {
                assert!(
                    seen_workspace_id.insert(workspace.id()),
                    "workspace id must be unique"
                );

                if let Some(name) = &workspace.name {
                    assert!(
                        !seen_workspace_name
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(name)),
                        "workspace name must be unique"
                    );
                    seen_workspace_name.push(name.clone());
                }

                workspace.verify_invariants(move_win_id.as_ref());

                let has_view_offset_gesture = workspace.scrolling().view_offset().is_gesture();
                if self.dnd.is_some() || self.interactive_move.is_some() {
                    // We'd like to check that all workspaces have the gesture here, furthermore we
                    // want to check that they have the gesture only if the interactive move
                    // targets the scrolling layout. However, we cannot do that because we start
                    // and stop the gesture lazily. Otherwise the gesture code would pollute a lot
                    // of places like adding new workspaces, implicitly moving windows between
                    // floating and tiling on fullscreen, etc.
                    //
                    // assert!(
                    //     has_view_offset_gesture,
                    //     "during an interactive move in the scrolling layout, \
                    //      all workspaces should be in a view offset gesture"
                    // );
                } else if saw_view_offset_gesture {
                    assert!(
                        !has_view_offset_gesture,
                        "only one workspace can have an ongoing view offset gesture"
                    );
                }
                saw_view_offset_gesture = has_view_offset_gesture;
            }
        }
    }

    pub fn advance_animations(&mut self) {
        let _span = tracy_client::span!("Layout::advance_animations");

        let mut dnd_scroll = None;
        let mut is_dnd = false;
        if let Some(dnd) = &self.dnd {
            dnd_scroll = Some((dnd.output.clone(), dnd.pointer_pos_within_output, true));
            is_dnd = true;
        }

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.advance_animations();

            if dnd_scroll.is_none() {
                // In GNOME mode a non-floating tile in flight is an overview
                // drag carrying a maximized window; the workspace views
                // shouldn't pan under it.
                let is_scrolling = !move_.is_floating
                    && self.options.layout.windowing_mode != WindowingMode::Floating;
                dnd_scroll = Some((
                    move_.output.clone(),
                    move_.pointer_pos_within_output,
                    is_scrolling,
                ));
            }
        }

        let is_overview_open = self.overview_open;

        // Scroll the view if needed.
        if let Some((output, pos_within_output, is_scrolling)) = dnd_scroll {
            if let Some(mon) = self.monitor_for_output_mut(&output) {
                let mut scrolled = false;

                let zoom = mon.overview_zoom();
                scrolled |= mon.dnd_scroll_gesture_scroll(pos_within_output, 1. / zoom);

                if is_scrolling {
                    if let Some((ws, geo)) = mon.workspace_under(pos_within_output) {
                        let ws_id = ws.id();
                        let ws = mon
                            .workspaces
                            .iter_mut()
                            .find(|ws| ws.id() == ws_id)
                            .unwrap();
                        // As far as the DnD scroll gesture is concerned, the workspace spans across
                        // the whole monitor horizontally.
                        let ws_pos = Point::from((0., geo.loc.y));
                        scrolled |=
                            ws.dnd_scroll_gesture_scroll(pos_within_output - ws_pos, 1. / zoom);
                    }
                }

                if scrolled {
                    // Don't trigger DnD hold while scrolling.
                    if let Some(dnd) = &mut self.dnd {
                        dnd.hold = None;
                    }
                } else if is_dnd {
                    let target = mon
                        .window_under(pos_within_output)
                        .map(|(win, _)| DndHoldTarget::Window(win.id().clone()))
                        .or_else(|| {
                            mon.workspace_under_narrow(pos_within_output)
                                .map(|ws| DndHoldTarget::Workspace(ws.id()))
                        });

                    let dnd = self.dnd.as_mut().unwrap();
                    if let Some(target) = target {
                        let now = self.clock.now_unadjusted();
                        let start_time = if let Some(hold) = &mut dnd.hold {
                            if hold.target != target {
                                hold.start_time = now;
                            }
                            hold.target = target;
                            hold.start_time
                        } else {
                            let hold = dnd.hold.insert(DndHold {
                                start_time: now,
                                target,
                            });
                            hold.start_time
                        };

                        // Delay copied from gnome-shell.
                        let delay = Duration::from_millis(750);
                        if delay <= now.saturating_sub(start_time) {
                            let hold = dnd.hold.take().unwrap();

                            // Synchronize workspace switch to overview close to get a monotonic
                            // animation.
                            let config = is_overview_open
                                .then_some(self.options.animations.overview_open_close.0);

                            let mon = self.monitor_for_output_mut(&output).unwrap();

                            let ws_idx = match hold.target {
                                DndHoldTarget::Window(id) => mon
                                    .workspaces
                                    .iter_mut()
                                    .position(|ws| ws.activate_window(&id))
                                    .unwrap(),
                                DndHoldTarget::Workspace(id) => {
                                    mon.workspaces.iter().position(|ws| ws.id() == id).unwrap()
                                }
                            };

                            mon.dnd_scroll_gesture_end();
                            mon.activate_workspace_with_anim_config(ws_idx, config);

                            self.focus_output(&output);

                            if is_overview_open {
                                self.close_overview();
                            }
                        }
                    } else {
                        // No target, reset the hold timer.
                        dnd.hold = None;
                    }
                }
            }
        }

        let mut overview_hidden = false;
        if let Some(OverviewProgress::Animation(anim)) = &mut self.overview_progress {
            if anim.is_done() {
                if self.overview_open {
                    self.overview_progress = Some(OverviewProgress::Open);
                } else {
                    self.overview_progress = None;
                    // Fully hidden: snap the frozen show-apps state back to the
                    // picker so the next open starts there.
                    self.app_grid_open = false;
                    overview_hidden = true;
                }
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.set_overview_progress(self.overview_progress.as_ref());
                    if overview_hidden {
                        mon.reset_app_grid();
                    }
                    mon.advance_animations();
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.advance_animations();
                }
            }
        }
    }

    pub fn are_animations_ongoing(&self, output: Option<&Output>) -> bool {
        !self.animation_causes(output).is_empty()
    }

    /// The same predicates as [`Self::are_animations_ongoing`], but keeping *which*
    /// ones fired so the frame log can name what a frame was animating. The bool is
    /// derived from this set rather than accumulated beside it, so a new animation
    /// cannot be added to one and forgotten in the other.
    pub fn animation_causes(&self, output: Option<&Output>) -> AnimCauses {
        let mut causes = AnimCauses::empty();

        // Keep advancing animations if we might need to scroll the view.
        if let Some(dnd) = &self.dnd {
            if output.is_none_or(|output| *output == dnd.output) {
                causes |= AnimCauses::DND;
            }
        }

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if output.is_none_or(|output| *output == move_.output) {
                if move_.tile.are_animations_ongoing() {
                    causes |= AnimCauses::INTERACTIVE_MOVE;
                }

                // Keep advancing animations if we might need to scroll the view.
                if !move_.is_floating || self.overview_open {
                    causes |= AnimCauses::INTERACTIVE_MOVE;
                }
            }
        }

        if self
            .overview_progress
            .as_ref()
            .is_some_and(|p| p.is_animation())
        {
            causes |= AnimCauses::OVERVIEW;
        }

        for mon in self.monitors() {
            if output.is_some_and(|output| mon.output != *output) {
                continue;
            }

            causes |= mon.animation_causes();
        }

        causes
    }

    pub fn update_render_elements(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_render_elements");

        self.update_render_elements_time = self.clock.now();

        let zoom = self.interactive_move_zoom();
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if output.is_none_or(|output| move_.output == *output) {
                let pos_within_output = move_.tile_render_location(zoom);

                // We're not on any specific workspace so we can't compute a "workspace view" rect.
                // Let's instead compute a rect relative to the output.
                //
                // FIXME: we could make the colors match up better in the overview by figuring out
                // where a centered workspace would currently be, and computing the view rect
                // against that. Since most of the time the dragged window will be on a centered
                // workspace.
                let view_rect =
                    Rectangle::new(pos_within_output.upscale(-1.), output_size(&move_.output))
                        .downscale(zoom);

                move_.tile.update_render_elements(true, view_rect);
            }
        }

        self.update_insert_hint(output);

        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            if output.is_some() {
                error!("update_render_elements called with no monitors but Some output");
            }
            return;
        };

        for (idx, mon) in monitors.iter_mut().enumerate() {
            if output.is_none_or(|output| mon.output == *output) {
                let is_active = self.is_active
                    && idx == *active_monitor_idx
                    && !matches!(self.interactive_move, Some(InteractiveMoveState::Moving(_)));
                mon.set_overview_progress(self.overview_progress.as_ref());
                mon.update_render_elements(is_active);
            }
        }
    }

    pub fn update_shaders(&mut self) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            move_.tile.update_shaders();
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.update_shaders();
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.update_shaders();
                }
            }
        }
    }

    fn update_insert_hint(&mut self, output: Option<&Output>) {
        let _span = tracy_client::span!("Layout::update_insert_hint");

        for mon in self.monitors_mut() {
            mon.insert_hint = None;
        }

        if !matches!(self.interactive_move, Some(InteractiveMoveState::Moving(_))) {
            return;
        }
        let Some(InteractiveMoveState::Moving(move_)) = self.interactive_move.take() else {
            unreachable!()
        };
        if output.is_some_and(|out| &move_.output != out) {
            self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            return;
        }

        let _span = tracy_client::span!("Layout::update_insert_hint::update");

        // Dropping on a screen edge tiles/maximizes (mutter edge tiling), but
        // not when dragging within the overview.
        let edge_tiling = self.gnome_edge_tiling && !self.overview_open;

        if let Some(mon) = self.monitor_for_output_mut(&move_.output) {
            let zoom = mon.overview_zoom();
            // Note: the hint was cleared above, so this hit-tests the strip
            // at rest; hovering the (wider) placeholder area keeps mapping
            // to the same gap, so the hover is stable.
            let via_strip = mon
                .thumbnail_strip()
                .is_some_and(|strip| strip.drop_target(move_.pointer_pos_within_output).is_some());
            let (insert_ws, geo) = mon.insert_position(move_.pointer_pos_within_output);
            match insert_ws {
                InsertWorkspace::Existing(ws_id) => {
                    let ws = mon
                        .workspaces
                        .iter_mut()
                        .find(|ws| ws.id() == ws_id)
                        .unwrap();
                    let pos_within_workspace =
                        (move_.pointer_pos_within_output - geo.loc).downscale(zoom);
                    let position = if move_.is_floating {
                        let target = edge_tiling
                            .then(|| ws.edge_tile_target(pos_within_workspace))
                            .flatten();
                        target.map_or(InsertPosition::Floating, InsertPosition::EdgeTile)
                    } else {
                        ws.scrolling_insert_position(pos_within_workspace)
                    };

                    let border_width = move_.tile.effective_border_width().unwrap_or(0.);
                    let corner_radius = move_
                        .tile
                        .window()
                        .geometry_corner_radius()
                        .expanded_by(border_width as f32);
                    mon.insert_hint = Some(InsertHint {
                        workspace: insert_ws,
                        position,
                        corner_radius,
                        via_strip,
                    });
                }
                InsertWorkspace::NewAt(_) => {
                    let position = if move_.is_floating {
                        InsertPosition::Floating
                    } else {
                        InsertPosition::NewColumn(0)
                    };
                    mon.insert_hint = Some(InsertHint {
                        workspace: insert_ws,
                        position,
                        corner_radius: CornerRadius::default(),
                        via_strip,
                    });
                }
            }
        }

        self.interactive_move = Some(InteractiveMoveState::Moving(move_));
    }

    pub fn ensure_named_workspace(&mut self, ws_config: &WorkspaceConfig) {
        if self.find_workspace_by_name(&ws_config.name.0).is_some() {
            return;
        }

        let clock = self.clock.clone();
        let options = self.options.clone();

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                primary_idx,
                active_monitor_idx,
            } => {
                let mon_idx = ws_config
                    .open_on_output
                    .as_deref()
                    .map(|name| {
                        monitors
                            .iter_mut()
                            .position(|monitor| output_matches_name(&monitor.output, name))
                            .unwrap_or(*primary_idx)
                    })
                    .unwrap_or(*active_monitor_idx);
                let mon = &mut monitors[mon_idx];

                let ws = Workspace::new_with_config(
                    mon.output.clone(),
                    Some(ws_config.clone()),
                    clock,
                    options,
                );
                mon.insert_workspace(ws, 0, false);
            }
            MonitorSet::NoOutputs { workspaces } => {
                let ws =
                    Workspace::new_with_config_no_outputs(Some(ws_config.clone()), clock, options);
                workspaces.insert(0, ws);
            }
        }
    }

    pub fn update_config(&mut self, config: &Config) {
        // Update workspace-specific config for all named workspaces.
        for ws in self.workspaces_mut() {
            let Some(name) = ws.name() else { continue };
            if let Some(config) = config.workspaces.iter().find(|w| &w.name.0 == name) {
                ws.update_layout_config(config.layout.clone().map(|x| x.0));
            }
        }

        // The `gnome_*` options come from GSettings, not from the config, so a
        // config reload must not reset them to the schema defaults.
        let mut options = Options::from_config(config);
        options.gnome_center_new_windows = self.options.gnome_center_new_windows;
        options.gnome_auto_maximize = self.options.gnome_auto_maximize;
        self.update_options(options);
    }

    fn update_options(&mut self, options: Options) {
        let options = Rc::new(options);

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            let view_size = output_size(&move_.output);
            let scale = move_.output.current_scale().fractional_scale();
            let options = Options::clone(&options)
                .with_merged_layout(move_.output_config.as_ref())
                .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                .adjusted_for_scale(scale);
            move_.tile.update_config(view_size, scale, Rc::new(options));
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    mon.update_config(options.clone());
                }
            }
            MonitorSet::NoOutputs { workspaces } => {
                for ws in workspaces {
                    ws.update_config(options.clone());
                }
            }
        }

        self.options = options;
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.toggle_width(forwards);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_width(window, forwards);
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_height(window, forwards);
    }

    pub fn toggle_full_width(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.toggle_full_width();
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.set_column_width(change);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.set_window_width(window, change);
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.set_window_height(window, change);
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.reset_window_height(window);
    }

    pub fn expand_column_to_available_width(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.expand_column_to_available_width();
    }

    pub fn toggle_window_floating(&mut self, window: Option<&W::Id>) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                move_.is_floating = !move_.is_floating;

                // When going to floating, restore the floating window size.
                if move_.is_floating {
                    let floating_size = move_.tile.floating_window_size;
                    let win = move_.tile.window_mut();
                    let mut size =
                        floating_size.unwrap_or_else(|| win.expected_size().unwrap_or_default());

                    // Apply min/max size window rules. If requesting a concrete size, apply
                    // completely; if requesting (0, 0), apply only when min/max results in a fixed
                    // size.
                    let min_size = win.min_size();
                    let max_size = win.max_size();
                    size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
                    size.h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);

                    win.request_size_once(size, true);

                    // Animate the tile back to opaque.
                    move_.tile.animate_alpha(
                        INTERACTIVE_MOVE_ALPHA,
                        1.,
                        self.options.animations.window_movement.0,
                    );

                    // Unlock the view on the workspaces.
                    for ws in self.workspaces_mut() {
                        ws.dnd_scroll_gesture_end();
                    }
                } else {
                    // Animate the tile back to semitransparent.
                    move_.tile.animate_alpha(
                        1.,
                        INTERACTIVE_MOVE_ALPHA,
                        self.options.animations.window_movement.0,
                    );
                    move_.tile.hold_alpha_animation_after_done();
                }

                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.toggle_window_floating(window);
    }

    pub fn set_window_floating(&mut self, window: Option<&W::Id>, floating: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                if move_.is_floating != floating {
                    self.toggle_window_floating(window);
                }
                return;
            }
        }

        let workspace = if let Some(window) = window {
            Some(
                self.workspaces_mut()
                    .find(|ws| ws.has_window(window))
                    .unwrap(),
            )
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.set_window_floating(window, floating);
    }

    pub fn focus_floating(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_floating();
    }

    pub fn focus_tiling(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.focus_tiling();
    }

    pub fn switch_focus_floating_tiling(&mut self) {
        let Some(workspace) = self.active_workspace_mut() else {
            return;
        };
        workspace.switch_focus_floating_tiling();
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if id.is_none() || id == Some(move_.tile.window().id()) {
                return;
            }
        }

        let workspace = if let Some(id) = id {
            Some(self.workspaces_mut().find(|ws| ws.has_window(id)).unwrap())
        } else {
            self.active_workspace_mut()
        };

        let Some(workspace) = workspace else {
            return;
        };
        workspace.move_floating_window(id, x, y, animate);
    }

    pub fn focus_output(&mut self, output: &Output) {
        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            for (idx, mon) in monitors.iter().enumerate() {
                if &mon.output == output {
                    *active_monitor_idx = idx;
                    return;
                }
            }
        }
    }

    pub fn move_to_output(
        &mut self,
        window: Option<&W::Id>,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: ActivateWindow,
    ) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if window.is_none() || window == Some(move_.tile.window().id()) {
                return;
            }
        }

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let new_idx = monitors
                .iter()
                .position(|mon| &mon.output == output)
                .unwrap();

            let (mon_idx, ws_idx) = if let Some(window) = window {
                monitors
                    .iter()
                    .enumerate()
                    .find_map(|(mon_idx, mon)| {
                        mon.workspaces
                            .iter()
                            .position(|ws| ws.has_window(window))
                            .map(|ws_idx| (mon_idx, ws_idx))
                    })
                    .unwrap()
            } else {
                let mon_idx = *active_monitor_idx;
                let mon = &monitors[mon_idx];
                (mon_idx, mon.active_workspace_idx)
            };

            let workspace_idx = target_ws_idx.unwrap_or(monitors[new_idx].active_workspace_idx);
            if mon_idx == new_idx && ws_idx == workspace_idx {
                return;
            }

            let mon = &monitors[new_idx];
            if mon.workspaces.len() <= workspace_idx {
                return;
            }

            let ws_id = mon.workspaces[workspace_idx].id();

            let mon = &mut monitors[mon_idx];
            let activate = activate.map_smart(|| {
                window.is_none_or(|win| {
                    mon_idx == *active_monitor_idx
                        && mon.active_window().map(|win| win.id()) == Some(win)
                })
            });
            let activate = if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            };

            let ws = &mut mon.workspaces[ws_idx];
            let transaction = Transaction::new();
            let mut removed = if let Some(window) = window {
                ws.remove_tile(window, transaction)
            } else if let Some(removed) = ws.remove_active_tile(transaction) {
                removed
            } else {
                return;
            };

            removed.tile.stop_move_animations();

            let mon = &mut monitors[new_idx];
            mon.add_tile(
                removed.tile,
                MonitorAddWindowTarget::Workspace {
                    id: ws_id,
                    column_idx: None,
                },
                activate,
                true,
                removed.width,
                removed.is_full_width,
                removed.is_floating,
            );
            if activate.map_smart(|| false) {
                *active_monitor_idx = new_idx;
            }

            let mon = &mut monitors[mon_idx];
            if mon.workspace_switch.is_none() {
                monitors[mon_idx].clean_up_workspaces();
            }
        }
    }

    pub fn move_column_to_output(
        &mut self,
        output: &Output,
        target_ws_idx: Option<usize>,
        activate: bool,
    ) {
        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let new_idx = monitors
                .iter()
                .position(|mon| &mon.output == output)
                .unwrap();

            let current = &mut monitors[*active_monitor_idx];
            let ws = current.active_workspace();

            if ws.floating_is_active() {
                self.move_to_output(None, output, None, ActivateWindow::Smart);
                return;
            }

            let Some(column) = ws.remove_active_column() else {
                return;
            };

            let workspace_idx = target_ws_idx
                .unwrap_or(monitors[new_idx].active_workspace_idx)
                .min(monitors[new_idx].workspaces.len() - 1);
            self.add_column_by_idx(new_idx, workspace_idx, column, activate);
        }
    }

    pub fn move_workspace_to_output(&mut self, output: &Output) -> bool {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &self.monitor_set
        else {
            return false;
        };

        let idx = monitors[*active_monitor_idx].active_workspace_idx;
        self.move_workspace_to_output_by_id(idx, None, output)
    }

    // FIXME: accept workspace by id
    pub fn move_workspace_to_output_by_id(
        &mut self,
        old_idx: usize,
        old_output: Option<Output>,
        new_output: &Output,
    ) -> bool {
        let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        else {
            return false;
        };

        let current_idx = if let Some(old_output) = old_output {
            monitors
                .iter()
                .position(|mon| mon.output == old_output)
                .unwrap()
        } else {
            *active_monitor_idx
        };
        let target_idx = monitors
            .iter()
            .position(|mon| mon.output == *new_output)
            .unwrap();

        let current = &mut monitors[current_idx];

        if current.workspaces.len() <= old_idx {
            return false;
        }

        // Do not do anything if the output is already correct
        if current_idx == target_idx {
            // Just update the original output since this is an explicit movement action.
            current.workspaces[old_idx].original_output = OutputId::new(&current.output);

            return false;
        }

        // Only switch active monitor if the workspace to be moved is the currently focused one on
        // the current monitor.
        let activate =
            current_idx == *active_monitor_idx && old_idx == current.active_workspace_idx;

        let mut ws = current.remove_workspace_by_idx(old_idx);
        ws.original_output = OutputId::new(new_output);

        let target = &mut monitors[target_idx];
        target.insert_workspace(ws, target.active_workspace_idx + 1, activate);

        if activate {
            *active_monitor_idx = target_idx;
        }

        activate
    }

    pub fn set_fullscreen(&mut self, id: &W::Id, is_fullscreen: bool) {
        // Check if this is a request to unset the windowed fullscreen state.
        if !is_fullscreen {
            let mut handled = false;
            self.with_windows_mut(|window, _| {
                if window.id() == id && window.is_pending_windowed_fullscreen() {
                    window.request_windowed_fullscreen(false);
                    handled = true;
                }
            });
            if handled {
                return;
            }
        }

        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == id {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.set_fullscreen(id, is_fullscreen);
                return;
            }
        }
    }

    pub fn toggle_fullscreen(&mut self, id: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == id {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.toggle_fullscreen(id);
                return;
            }
        }
    }

    pub fn toggle_windowed_fullscreen(&mut self, id: &W::Id) {
        let (_, window) = self.windows().find(|(_, win)| win.id() == id).unwrap();
        if window.pending_sizing_mode().is_fullscreen() {
            // Remove the real fullscreen.
            for ws in self.workspaces_mut() {
                if ws.has_window(id) {
                    ws.set_fullscreen(id, false);
                    break;
                }
            }
        }

        // This will switch is_pending_fullscreen() to false right away.
        self.with_windows_mut(|window, _| {
            if window.id() == id {
                window.request_windowed_fullscreen(!window.is_pending_windowed_fullscreen());
            }
        });
    }

    pub fn set_maximized(&mut self, id: &W::Id, maximize: bool) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == id {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.set_maximized(id, maximize);
                return;
            }
        }
    }

    /// Tiles the window to the given half of the work area, or untiles it if
    /// already tiled there (GNOME Super+Left/Right).
    pub fn toggle_tiled(&mut self, id: &W::Id, side: TileSide) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == id {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.toggle_tiled(Some(id), side);
                return;
            }
        }
    }

    /// mutter's denied-focus placement (place.c:1052-1086): keeps a window
    /// that was refused focus from covering the window that kept it.
    ///
    /// Returns whether the window moved.
    pub fn avoid_focus_window(&mut self, id: &W::Id) -> bool {
        let Some(focus) = self.focus().map(|win| win.id().clone()) else {
            return false;
        };
        if focus == *id {
            return false;
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                return ws.avoid_focus_window(id, &focus);
            }
        }
        false
    }

    /// mutter's map-time auto-maximize: maximizes the window if it covers
    /// more than 80% of the work area, and `org.gnome.mutter auto-maximize`
    /// allows it (place.c:1088).
    pub fn auto_maximize_if_too_big(&mut self, id: &W::Id) -> bool {
        if !self.options.gnome_auto_maximize {
            return false;
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                return ws.auto_maximize_if_too_big(id);
            }
        }
        false
    }

    pub fn toggle_maximized(&mut self, id: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == id {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.has_window(id) {
                ws.toggle_maximized(id);
                return;
            }
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, output: &Output, is_touchpad: bool) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => unreachable!(),
        };

        for monitor in monitors {
            // Cancel the gesture on other outputs.
            if &monitor.output != output {
                monitor.workspace_switch_gesture_end(None);
                continue;
            }

            monitor.workspace_switch_gesture_begin(is_touchpad);
        }
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<Option<Output>> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            if let Some(refresh) =
                monitor.workspace_switch_gesture_update(delta_y, timestamp, is_touchpad)
            {
                if refresh {
                    return Some(Some(monitor.output.clone()));
                } else {
                    return Some(None);
                }
            }
        }

        None
    }

    pub fn workspace_switch_gesture_end(&mut self, is_touchpad: Option<bool>) -> Option<Output> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            if monitor.workspace_switch_gesture_end(is_touchpad) {
                return Some(monitor.output.clone());
            }
        }

        None
    }

    pub fn view_offset_gesture_begin(
        &mut self,
        output: &Output,
        workspace_idx: Option<usize>,
        is_touchpad: bool,
    ) {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => unreachable!(),
        };

        for monitor in monitors {
            for (idx, ws) in monitor.workspaces.iter_mut().enumerate() {
                // Cancel the gesture on other workspaces.
                if &monitor.output != output
                    || idx != workspace_idx.unwrap_or(monitor.active_workspace_idx)
                {
                    ws.view_offset_gesture_end(None);
                    continue;
                }

                ws.view_offset_gesture_begin(is_touchpad);
            }
        }
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<Option<Output>> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            // The zoom follows each monitor's own overview chrome.
            let delta_x = delta_x / monitor.overview_zoom();
            for ws in &mut monitor.workspaces {
                if let Some(refresh) =
                    ws.view_offset_gesture_update(delta_x, timestamp, is_touchpad)
                {
                    if refresh {
                        return Some(Some(monitor.output.clone()));
                    } else {
                        return Some(None);
                    }
                }
            }
        }

        None
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> Option<Output> {
        let monitors = match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => monitors,
            MonitorSet::NoOutputs { .. } => return None,
        };

        for monitor in monitors {
            for ws in &mut monitor.workspaces {
                if ws.view_offset_gesture_end(is_touchpad) {
                    return Some(monitor.output.clone());
                }
            }
        }

        None
    }

    pub fn overview_gesture_begin(&mut self) {
        self.overview_open = true;

        let value = self.overview_progress.take().map_or(0., |p| p.value());
        let gesture = OverviewGesture {
            tracker: SwipeTracker::new(),
            start: value,
            value,
        };
        self.overview_progress = Some(OverviewProgress::Gesture(gesture));

        self.set_monitors_overview_state();
    }

    pub fn overview_gesture_update(&mut self, delta_y: f64, timestamp: Duration) -> Option<bool> {
        let Some(OverviewProgress::Gesture(gesture)) = &mut self.overview_progress else {
            return None;
        };

        gesture.tracker.push(delta_y, timestamp);

        let total_height = OVERVIEW_GESTURE_MOVEMENT;
        let pos = gesture.tracker.pos() / total_height;
        let new_value = gesture.start + pos;
        let new_value = OVERVIEW_GESTURE_RUBBER_BAND.clamp(0., 1., new_value);

        if gesture.value == new_value {
            return Some(false);
        }

        gesture.value = new_value;
        self.set_monitors_overview_state();

        Some(true)
    }

    pub fn overview_gesture_end(&mut self) -> bool {
        let Some(OverviewProgress::Gesture(gesture)) = &mut self.overview_progress else {
            return false;
        };

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let total_height = OVERVIEW_GESTURE_MOVEMENT;

        let mut velocity = gesture.tracker.velocity() / total_height;
        let current_pos = gesture.tracker.pos() / total_height;
        let pos = gesture.tracker.projected_end_pos() / total_height;

        let new_value = gesture.start + pos;
        let new_value = new_value.clamp(0., 1.).round();

        velocity *=
            OVERVIEW_GESTURE_RUBBER_BAND.clamp_derivative(0., 1., gesture.start + current_pos);

        self.overview_open = new_value == 1.;
        self.overview_progress = Some(OverviewProgress::Animation(Animation::new(
            self.clock.clone(),
            gesture.value,
            new_value,
            velocity,
            self.options.animations.overview_open_close.0,
        )));

        self.set_monitors_overview_state();

        true
    }

    /// Show `windows` above every other window while a switcher is up, topmost first.
    ///
    /// Only the windows sharing the *first* one's workspace are promoted, the same restriction
    /// `shell_app_activate_window` puts on its raise (`shell-app.c:413-415`): the preview is a
    /// rehearsal of the commit, so it must not promise a stacking the commit will not perform.
    ///
    /// The clearing half always runs over every workspace — a preview that ends on another
    /// monitor must not leave a window pinned on the one it started on — which is why an empty
    /// slice is the way to drop a preview, and why this must be called even with nothing open.
    pub fn set_preview_raised(&mut self, windows: &[W::Id]) {
        let on_workspace = windows.first().and_then(|first| {
            self.workspaces()
                .find(|(_, _, ws)| ws.has_window(first))
                .map(|(_, _, ws)| {
                    let ids: Vec<W::Id> = windows
                        .iter()
                        .filter(|id| ws.has_window(id))
                        .cloned()
                        .collect();
                    (ws.id(), ids)
                })
        });

        for ws in self.workspaces_mut() {
            match &on_workspace {
                Some((ws_id, ids)) if ws.id() == *ws_id => ws.set_preview_raised(ids),
                _ => ws.set_preview_raised(&[]),
            }
        }
    }

    /// Take `window`'s monitor to `window`'s workspace for a *preview*, reporting where it was so
    /// the session can put it back. `None` when it is already there, or the window is gone.
    ///
    /// DIVERGENCE: this is a real workspace switch, animation and all — a preview of the commit,
    /// performed rather than mimed, so what you are looking at while you tab is what you get. The
    /// one piece of state it must not disturb is the `previous_workspace_id` bookmark: a preview
    /// you abandoned is not somewhere you "were", and "switch to previous workspace" must not
    /// learn about the workspaces a switcher merely passed through.
    pub fn preview_workspace_of(&mut self, window: &W::Id) -> Option<WorkspacePreviewOrigin> {
        let (output, target_idx) = self.monitors().find_map(|mon| {
            let idx = mon.workspaces.iter().position(|ws| ws.has_window(window))?;
            Some((mon.output().clone(), idx))
        })?;

        let mon = self.monitor_for_output_mut(&output)?;
        if mon.active_workspace_idx() == target_idx {
            return None;
        }

        let origin = WorkspacePreviewOrigin {
            output,
            idx: mon.active_workspace_idx(),
            id: mon.workspaces[mon.active_workspace_idx()].id(),
            previous: mon.previous_workspace_id,
        };
        mon.switch_workspace(target_idx);
        Some(origin)
    }

    /// Put a monitor back the way [`preview_workspace_of`](Self::preview_workspace_of) found it,
    /// animating the way it came — the abandoned-session half.
    pub fn undo_workspace_preview(&mut self, origin: &WorkspacePreviewOrigin) {
        let Some(mon) = self.monitor_for_output_mut(&origin.output) else {
            return;
        };
        // By id first: the strip can move under a preview — `w`/`F4` closes windows without
        // ending the session, and emptying the workspace the switcher started on is enough to
        // renumber it. The recorded index is only the fallback for a workspace that is gone
        // outright, where landing next to where you were beats not moving at all.
        let idx = mon
            .workspaces
            .iter()
            .position(|ws| ws.id() == origin.id)
            .unwrap_or(origin.idx);
        mon.switch_workspace(idx);
        mon.previous_workspace_id = origin.previous;
    }

    /// Keep where the preview went, but point the bookmark at where the session *started* — the
    /// committed half. Otherwise "switch to previous workspace" lands on whichever workspace the
    /// tabbing happened to rest on last, which is not a place you have ever been.
    pub fn keep_workspace_preview(&mut self, origin: &WorkspacePreviewOrigin) {
        let Some(mon) = self.monitor_for_output_mut(&origin.output) else {
            return;
        };
        mon.previous_workspace_id = Some(origin.id);
    }

    /// Which output `id` is on, for deciding whose preview a commit gets to keep.
    pub fn output_of_window(&self, id: &W::Id) -> Option<Output> {
        self.monitors()
            .find(|mon| mon.has_window(id))
            .map(|mon| mon.output().clone())
    }

    /// Where `id` is drawn on `output` right now, for the `.cycler-highlight` border the shell
    /// puts around it. `None` when the window lives on another output.
    ///
    /// Re-derived rather than remembered, because the window can move or resize under a switcher
    /// that is still up.
    pub fn window_render_rect(
        &self,
        id: &W::Id,
        output: &Output,
    ) -> Option<Rectangle<f64, Logical>> {
        let (mon, (ws, ws_geo)) = self.monitors().find_map(|mon| {
            mon.workspaces_with_render_geo()
                .find(|(ws, _)| ws.has_window(id))
                .map(|rv| (mon, rv))
        })?;
        if mon.output() != output {
            return None;
        }

        let zoom = mon.overview_zoom();
        let (tile, tile_offset, _visible) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == id)?;

        // The window's own rect, not the tile's: the tile carries the border/shadow slack that
        // GNOME's `get_buffer_rect` also excludes (`_onSizeChanged`, `altTab.js:465-472`).
        let loc = ws_geo.loc + (tile_offset + tile.window_loc()).upscale(zoom);
        Some(Rectangle::new(loc, tile.window_size().upscale(zoom)))
    }

    pub fn interactive_move_begin(
        &mut self,
        window_id: W::Id,
        output: &Output,
        start_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.interactive_move.is_some() {
            return false;
        }

        let Some((mon, (ws, ws_geo))) = self.monitors().find_map(|mon| {
            mon.workspaces_with_render_geo()
                .find(|(ws, _)| ws.has_window(&window_id))
                .map(|rv| (mon, rv))
        }) else {
            return false;
        };

        if mon.output() != output {
            return false;
        }

        let zoom = mon.overview_zoom();

        let is_floating = ws.is_floating(&window_id);
        let (tile, tile_offset, _visible) = ws
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == &window_id)
            .unwrap();
        let window_offset = tile.window_loc();

        let tile_pos = ws_geo.loc + tile_offset.upscale(zoom);

        // In the GNOME overview the grab is on the picker preview, so the
        // grab point is measured against the preview's slot on screen, not
        // the window's real rect.
        let expose_slot = mon
            .expose_progress()
            .is_some()
            .then(|| ws.expose_slot(&window_id))
            .flatten()
            .map(|slot| {
                Rectangle::new(ws_geo.loc + slot.loc.upscale(zoom), slot.size.upscale(zoom))
            });

        let pointer_ratio_within_window = if let Some(slot) = expose_slot {
            let offset = start_pos_within_output - slot.loc;
            (
                f64::clamp(offset.x / slot.size.w, 0., 1.),
                f64::clamp(offset.y / slot.size.h, 0., 1.),
            )
        } else {
            let pointer_offset_within_window =
                start_pos_within_output - tile_pos - window_offset.upscale(zoom);
            let window_size = tile.window_size().upscale(zoom);
            (
                f64::clamp(pointer_offset_within_window.x / window_size.w, 0., 1.),
                f64::clamp(pointer_offset_within_window.y / window_size.h, 0., 1.),
            )
        };

        self.interactive_move = Some(InteractiveMoveState::Starting {
            window_id,
            pointer_delta: Point::from((0., 0.)),
            pointer_ratio_within_window,
        });

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_begin();
        }

        // Lock the view for scrolling interactive move.
        if !is_floating {
            for ws in self.workspaces_mut() {
                ws.dnd_scroll_gesture_begin();
            }
        }

        true
    }

    pub fn interactive_move_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
        output: Output,
        pointer_pos_within_output: Point<f64, Logical>,
    ) -> bool {
        let Some(state) = self.interactive_move.take() else {
            return false;
        };

        match state {
            InteractiveMoveState::Starting {
                window_id,
                mut pointer_delta,
                pointer_ratio_within_window,
            } => {
                if window_id != *window {
                    self.interactive_move = Some(InteractiveMoveState::Starting {
                        window_id,
                        pointer_delta,
                        pointer_ratio_within_window,
                    });
                    return false;
                }

                let zoom = self.overview_zoom_for_output(&output);
                let delta = delta.downscale(zoom);

                pointer_delta += delta;

                let (cx, cy) = (pointer_delta.x, pointer_delta.y);
                let sq_dist = cx * cx + cy * cy;

                let gnome_mode = self.options.layout.windowing_mode == WindowingMode::Floating;
                // In the overview the drag grabs a picker preview, which starts
                // moving right away — there's nothing to shake loose from
                // (gnome-shell's WindowPreview drags).
                let in_expose = gnome_mode && self.overview_open;

                let (is_floating, tile, workspace_config) = self
                    .workspaces_mut()
                    .find(|ws| ws.has_window(&window_id))
                    .map(|ws| {
                        let workspace_config = ws.layout_config().cloned().map(|c| (ws.id(), c));
                        (
                            ws.is_floating(&window_id),
                            ws.tiles_mut()
                                .find(|tile| *tile.window().id() == window_id)
                                .unwrap(),
                            workspace_config,
                        )
                    })
                    .unwrap();
                // The rubberband is the *visual* for a shake-loose threshold:
                // the window trails the pointer with resistance so you can see
                // you are pulling on something. In the overview there is no
                // threshold — `started` below is unconditionally true — so the
                // offset would be written and cleared inside this one call,
                // having been resistance nobody saw.
                //
                // gnome-shell's WindowPreview drag never moves the window in the
                // workspace layout at all. (`expose_layout` now subtracts
                // `Tile::render_offset()` before laying out, so an offset here
                // would no longer re-sort the picker either — but writing one
                // nobody can see is still pointless work.)
                if !in_expose {
                    let factor = RubberBand {
                        stiffness: 1.0,
                        limit: 0.5,
                    }
                    .band(sq_dist / INTERACTIVE_MOVE_START_THRESHOLD);
                    tile.interactive_move_offset = pointer_delta.upscale(factor);
                }
                let is_edge_tiled = tile.window().edge_tiled_side().is_some();
                let is_expanded = !tile.window().pending_sizing_mode().is_normal();

                // Put it back to be able to easily return.
                self.interactive_move = Some(InteractiveMoveState::Starting {
                    window_id: window_id.clone(),
                    pointer_delta,
                    pointer_ratio_within_window,
                });

                // mutter shakes a maximized window loose after shake_threshold
                // px of *vertical* movement (dragging along the top edge keeps
                // it maximized, e.g. towards another monitor), and an
                // edge-tiled one after that much movement on either axis
                // (meta-window-drag.c, update_move). What shakes loose is a
                // property of the window's sizing mode, not of the layer it
                // sits in — in GNOME mode every window is floating, maximized
                // ones included.
                let started = if in_expose {
                    true
                } else if gnome_mode && is_expanded {
                    pointer_delta.y.abs() >= crate::gnome::SHAKE_THRESHOLD
                } else if !is_floating {
                    sq_dist >= INTERACTIVE_MOVE_START_THRESHOLD
                } else if gnome_mode && is_edge_tiled {
                    f64::max(pointer_delta.x.abs(), pointer_delta.y.abs())
                        >= crate::gnome::SHAKE_THRESHOLD
                } else {
                    true
                };
                if !started {
                    return true;
                }

                let output_config = self
                    .monitors()
                    .find(|mon| mon.output() == &output)
                    .and_then(|mon| mon.layout_config().cloned());

                // If the pointer is currently on the window's own output, then we can animate the
                // window movement from its current (rubberbanded and possibly moved away) position
                // to the pointer. Otherwise, we just teleport it as the layout code is not aware
                // of monitor positions.
                //
                // FIXME: when and if the layout code knows about monitor positions, this will be
                // potentially animatable.
                let mut tile_pos = None;
                let mut expose_pickup_size = None;
                if let Some((mon, (ws, ws_geo))) = self.monitors().find_map(|mon| {
                    mon.workspaces_with_render_geo()
                        .find(|(ws, _)| ws.has_window(window))
                        .map(|rv| (mon, rv))
                }) {
                    if mon.output() == &output {
                        let zoom = mon.overview_zoom();

                        // In the overview the tile renders as its picker
                        // preview; the drag continues from the slot, at the
                        // slot's size.
                        let expose_slot = (in_expose && mon.expose_progress().is_some())
                            .then(|| ws.expose_slot(window))
                            .flatten();
                        if let Some(slot) = expose_slot {
                            tile_pos = Some((ws_geo.loc + slot.loc.upscale(zoom), zoom));
                            expose_pickup_size = Some(slot.size.upscale(zoom));
                        } else {
                            let (_, tile_offset, _) = ws
                                .tiles_with_render_positions()
                                .find(|(tile, _, _)| tile.window().id() == window)
                                .unwrap();

                            tile_pos = Some((ws_geo.loc + tile_offset.upscale(zoom), zoom));
                        }
                    }
                }

                // Clear it before calling remove_window() to avoid running interactive_move_end()
                // in the middle of interactive_move_update() and the confusion that causes.
                self.interactive_move = None;

                let ws = self
                    .workspaces_mut()
                    .find(|ws| ws.has_window(&window_id))
                    .unwrap();
                if in_expose {
                    // gnome-shell's WindowPreview drag never touches the real
                    // window: no unmaximize/untile until the drop (the tile
                    // carries its state and restore rects along), and the
                    // source desktop's picker layout stays frozen so the
                    // remaining previews hold their slots (workspace.js
                    // layout_frozen).
                    ws.freeze_expose();
                } else {
                    // Unset fullscreen before removing the tile. This will restore its size
                    // properly, and move it to floating if needed, so we don't have to deal with
                    // that here.
                    ws.set_fullscreen(window, false);
                    ws.set_maximized(window, false);
                }

                let RemovedTile {
                    mut tile,
                    width,
                    is_full_width,
                    is_floating,
                } = self.remove_window(window, Transaction::new()).unwrap();

                tile.stop_move_animations();
                tile.interactive_move_offset = Point::from((0., 0.));
                tile.window().output_enter(&output);
                tile.window().set_preferred_scale_transform(
                    output.current_scale(),
                    output.current_transform(),
                );

                let view_size = output_size(&output);
                let scale = output.current_scale().fractional_scale();
                let options = Options::clone(&self.options)
                    .with_merged_layout(output_config.as_ref())
                    .with_merged_layout(workspace_config.as_ref().map(|(_, c)| c))
                    .adjusted_for_scale(scale);
                tile.update_config(view_size, scale, Rc::new(options));

                if is_floating {
                    // Unlock the view in case we locked it moving a fullscreen window that is
                    // going to unfullscreen to floating.
                    for ws in self.workspaces_mut() {
                        ws.dnd_scroll_gesture_end();
                    }
                } else {
                    // Animate to semitransparent.
                    tile.animate_alpha(
                        1.,
                        INTERACTIVE_MOVE_ALPHA,
                        self.options.animations.window_movement.0,
                    );
                    tile.hold_alpha_animation_after_done();
                }

                let mut data = InteractiveMoveData {
                    tile,
                    output,
                    pointer_pos_within_output,
                    width,
                    is_full_width,
                    is_floating,
                    pointer_ratio_within_window,
                    output_config,
                    workspace_config,
                    expose_pickup_size,
                    expose_dnd_shrink: expose_pickup_size.map(|_| {
                        Animation::new(
                            self.clock.clone(),
                            0.,
                            1.,
                            0.,
                            synoik_config::Animation {
                                off: self.options.animations.overview_open_close.0.off,
                                kind: synoik_config::animations::Kind::Easing(
                                    synoik_config::animations::EasingParams {
                                        duration_ms: DND_SCALE_ANIMATION_TIME_MS,
                                        curve: synoik_config::animations::Curve::EaseOutQuad,
                                    },
                                ),
                            },
                        )
                    }),
                };

                if let Some((tile_pos, zoom)) = tile_pos {
                    let new_tile_pos = data.tile_render_location(zoom);
                    data.tile
                        .animate_move_from((tile_pos - new_tile_pos).downscale(zoom));
                }

                self.interactive_move = Some(InteractiveMoveState::Moving(data));
            }
            InteractiveMoveState::Moving(mut move_) => {
                if window != move_.tile.window().id() {
                    self.interactive_move = Some(InteractiveMoveState::Moving(move_));
                    return false;
                }

                let mut ws_id = None;
                if let Some(mon) = self.monitor_for_output(&output) {
                    let (insert_ws, _) = mon.insert_position(move_.pointer_pos_within_output);
                    if let InsertWorkspace::Existing(id) = insert_ws {
                        ws_id = Some(id);
                    }
                }

                // If moved over a different workspace, reset the config override.
                let mut update_config = false;
                if let Some((id, _)) = &move_.workspace_config {
                    if Some(*id) != ws_id {
                        move_.workspace_config = None;
                        update_config = true;
                    }
                }

                if output != move_.output {
                    move_.tile.window().output_leave(&move_.output);
                    move_.tile.window().output_enter(&output);
                    move_.tile.window().set_preferred_scale_transform(
                        output.current_scale(),
                        output.current_transform(),
                    );
                    move_.output = output.clone();
                    self.focus_output(&output);

                    move_.output_config = self
                        .monitor_for_output(&output)
                        .and_then(|mon| mon.layout_config().cloned());

                    update_config = true;
                }

                if update_config {
                    let view_size = output_size(&output);
                    let scale = output.current_scale().fractional_scale();
                    let options = Options::clone(&self.options)
                        .with_merged_layout(move_.output_config.as_ref())
                        .with_merged_layout(move_.workspace_config.as_ref().map(|(_, c)| c))
                        .adjusted_for_scale(scale);
                    move_.tile.update_config(view_size, scale, Rc::new(options));
                }

                move_.pointer_pos_within_output = pointer_pos_within_output;

                self.interactive_move = Some(InteractiveMoveState::Moving(move_));
            }
        }

        true
    }

    pub fn interactive_move_end(&mut self, window: &W::Id) {
        let Some(move_) = &self.interactive_move else {
            return;
        };

        let move_ = match move_ {
            InteractiveMoveState::Starting { window_id, .. } => {
                if window_id != window {
                    return;
                }

                let Some(InteractiveMoveState::Starting { window_id, .. }) =
                    self.interactive_move.take()
                else {
                    unreachable!()
                };

                for mon in self.monitors_mut() {
                    mon.dnd_scroll_gesture_end();
                }

                for ws in self.workspaces_mut() {
                    if let Some(tile) = ws.tiles_mut().find(|tile| *tile.window().id() == window_id)
                    {
                        let offset = tile.interactive_move_offset;
                        tile.interactive_move_offset = Point::from((0., 0.));
                        tile.animate_move_from(offset);
                    }

                    // Unlock the view on the workspaces, but if the moved window was active,
                    // preserve that.
                    let moved_tile_was_active =
                        ws.active_window().is_some_and(|win| *win.id() == window_id);

                    ws.dnd_scroll_gesture_end();

                    if moved_tile_was_active {
                        ws.activate_window(&window_id);
                    }
                }

                return;
            }
            InteractiveMoveState::Moving(move_) => move_,
        };

        if window != move_.tile.window().id() {
            return;
        }

        let Some(InteractiveMoveState::Moving(mut move_)) = self.interactive_move.take() else {
            unreachable!()
        };

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_end();
        }

        // The drop lets the picker layouts recompute (frozen at pickup).
        for ws in self.workspaces_mut() {
            ws.unfreeze_expose();
        }

        // Unlock the view on the workspaces.
        if !move_.is_floating {
            for ws in self.workspaces_mut() {
                ws.dnd_scroll_gesture_end();
            }

            // Also animate the tile back to opaque.
            move_.tile.animate_alpha(
                INTERACTIVE_MOVE_ALPHA,
                1.,
                self.options.animations.window_movement.0,
            );
        }

        // Dragging in the overview shouldn't switch the workspace and so on.
        let allow_to_activate_workspace = !self.overview_open;
        // Dropping on a screen edge tiles/maximizes (mutter edge tiling), but
        // not from within the overview.
        let edge_tiling = self.gnome_edge_tiling && !self.overview_open;
        // A GNOME overview drag is gnome-shell's WindowPreview drag: the
        // preview's location isn't the window's, so dropping moves the window
        // between workspaces but never repositions it on its desktop.
        let keep_position =
            self.overview_open && self.options.layout.windowing_mode == WindowingMode::Floating;

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                let (mon, insert_ws, position, offset, zoom) = if let Some(mon) =
                    monitors.iter_mut().find(|mon| mon.output == move_.output)
                {
                    let zoom = mon.overview_zoom();

                    let (insert_ws, geo) = mon.insert_position(move_.pointer_pos_within_output);
                    let (position, offset) = match insert_ws {
                        InsertWorkspace::Existing(ws_id) => {
                            let ws_idx = mon
                                .workspaces
                                .iter_mut()
                                .position(|ws| ws.id() == ws_id)
                                .unwrap();

                            let pos_within_workspace =
                                (move_.pointer_pos_within_output - geo.loc).downscale(zoom);
                            let ws = &mut mon.workspaces[ws_idx];
                            // A picker drop re-adds the tile with its state
                            // untouched, whichever layer it came from.
                            let position = if move_.is_floating || keep_position {
                                let target = edge_tiling
                                    .then(|| ws.edge_tile_target(pos_within_workspace))
                                    .flatten();
                                target.map_or(InsertPosition::Floating, InsertPosition::EdgeTile)
                            } else {
                                ws.scrolling_insert_position(pos_within_workspace)
                            };

                            (position, Some(geo.loc))
                        }
                        InsertWorkspace::NewAt(_) => {
                            let position = if move_.is_floating || keep_position {
                                InsertPosition::Floating
                            } else {
                                InsertPosition::NewColumn(0)
                            };

                            (position, None)
                        }
                    };

                    (mon, insert_ws, position, offset, zoom)
                } else {
                    let mon = &mut monitors[*active_monitor_idx];
                    let zoom = mon.overview_zoom();
                    // No point in trying to use the pointer position on the wrong output.
                    let ws = &mon.workspaces[0];
                    let ws_geo = mon.workspaces_render_geo().next().unwrap();

                    let position = if move_.is_floating || keep_position {
                        InsertPosition::Floating
                    } else {
                        ws.scrolling_insert_position(Point::from((0., 0.)))
                    };

                    let insert_ws = InsertWorkspace::Existing(ws.id());
                    (mon, insert_ws, position, Some(ws_geo.loc), zoom)
                };

                let win_id = move_.tile.window().id().clone();
                let tile_render_loc = move_.tile_render_location(zoom);

                let ws_idx = match insert_ws {
                    InsertWorkspace::Existing(ws_id) => mon
                        .workspaces
                        .iter()
                        .position(|ws| ws.id() == ws_id)
                        .unwrap(),
                    InsertWorkspace::NewAt(ws_idx) => {
                        if mon.workspaces.len() - 1 <= ws_idx {
                            // Reuse the bottom empty workspace.
                            mon.workspaces.len() - 1
                        } else {
                            mon.add_workspace_at(ws_idx);
                            ws_idx
                        }
                    }
                };

                match position {
                    InsertPosition::NewColumn(column_idx) => {
                        let ws_id = mon.workspaces[ws_idx].id();
                        mon.add_tile(
                            move_.tile,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: Some(column_idx),
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            move_.width,
                            move_.is_full_width,
                            false,
                        );
                    }
                    InsertPosition::InColumn(column_idx, tile_idx) => {
                        mon.add_tile_to_column(
                            ws_idx,
                            column_idx,
                            Some(tile_idx),
                            move_.tile,
                            true,
                            allow_to_activate_workspace,
                        );
                    }
                    InsertPosition::Floating => {
                        let tile_render_loc = move_.tile_render_location(zoom);

                        let mut tile = move_.tile;

                        // The tile still carries its pre-drag position in
                        // floating_pos; a picker drop keeps it.
                        if !keep_position {
                            tile.floating_pos = None;

                            match insert_ws {
                                InsertWorkspace::Existing(_) => {
                                    if let Some(offset) = offset {
                                        let pos = (tile_render_loc - offset).downscale(zoom);
                                        let pos = mon.workspaces[ws_idx]
                                            .floating_logical_to_size_frac(pos);
                                        tile.floating_pos = Some(pos);
                                    } else {
                                        error!(
                                            "offset unset for inserting a floating tile \
                                             to existing workspace"
                                        );
                                    }
                                }
                                InsertWorkspace::NewAt(_) => {
                                    // When putting a floating tile on a new workspace, we don't
                                    // really have a good pre-existing position.
                                }
                            }
                        }

                        // Set the floating size so it takes into account any window resizing that
                        // took place during the move. Not on a picker drop: the window state was
                        // never touched, and for a maximized or edge-tiled window
                        // floating_window_size holds the restore size.
                        if !keep_position {
                            if let Some(size) = tile.window().expected_size() {
                                tile.floating_window_size = Some(size);
                            }
                        }

                        // A tile carrying maximized/fullscreen state lands
                        // back in the scrolling layout (see
                        // Workspace::add_tile), so a picker drop restores the
                        // window wholesale on the target desktop.
                        let ws_id = mon.workspaces[ws_idx].id();
                        mon.add_tile(
                            tile,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: None,
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            move_.width,
                            move_.is_full_width,
                            true,
                        );
                    }
                    InsertPosition::EdgeTile(target) => {
                        // Dropped on a screen edge: commit the tile/maximize.
                        // mutter uses the pre-drag geometry as the restore rect
                        // (end_grab_op: saved_rect = initial_window_pos); the
                        // tile still carries it in floating_pos and
                        // floating_window_size, so add it back as floating
                        // untouched and tile/maximize from there.
                        let ws_id = mon.workspaces[ws_idx].id();
                        mon.add_tile(
                            move_.tile,
                            MonitorAddWindowTarget::Workspace {
                                id: ws_id,
                                column_idx: None,
                            },
                            ActivateWindow::Yes,
                            allow_to_activate_workspace,
                            move_.width,
                            move_.is_full_width,
                            true,
                        );

                        let ws = mon
                            .workspaces
                            .iter_mut()
                            .find(|ws| ws.id() == ws_id)
                            .unwrap();
                        match target {
                            EdgeTileTarget::Tile(side) => ws.toggle_tiled(Some(&win_id), side),
                            EdgeTileTarget::Maximize => ws.set_maximized(&win_id, true),
                        }
                    }
                }

                // The insert above can shift the workspace index, so re-find the tile.
                let (tile, tile_offset, ws_geo) = mon
                    .workspaces_with_render_geo_mut(false)
                    .find_map(|(ws, geo)| {
                        ws.tiles_with_render_positions_mut(false)
                            .find(|(tile, _)| tile.window().id() == &win_id)
                            .map(|(tile, tile_offset)| (tile, tile_offset, geo))
                    })
                    .unwrap();
                let new_tile_render_loc = ws_geo.loc + tile_offset.upscale(zoom);

                tile.animate_move_from((tile_render_loc - new_tile_render_loc).downscale(zoom));
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                if workspaces.is_empty() {
                    workspaces.push(Workspace::new_no_outputs(
                        self.clock.clone(),
                        self.options.clone(),
                    ));
                }
                let ws = &mut workspaces[0];

                // No point in trying to use the pointer position without outputs.
                ws.add_tile(
                    move_.tile,
                    WorkspaceAddWindowTarget::Auto,
                    ActivateWindow::Yes,
                    move_.width,
                    move_.is_full_width,
                    move_.is_floating,
                );
            }
        }
    }

    pub fn interactive_move_is_moving_above_output(&self, output: &Output) -> bool {
        let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move else {
            return false;
        };

        move_.output == *output
    }

    pub fn dnd_update(&mut self, output: Output, pointer_pos_within_output: Point<f64, Logical>) {
        let begin_gesture = self.dnd.is_none();

        self.dnd = Some(DndData {
            output,
            pointer_pos_within_output,
            hold: None,
        });

        if begin_gesture {
            for mon in self.monitors_mut() {
                mon.dnd_scroll_gesture_begin();
            }

            for ws in self.workspaces_mut() {
                ws.dnd_scroll_gesture_begin();
            }
        }
    }

    pub fn dnd_end(&mut self) {
        if self.dnd.is_none() {
            return;
        }

        self.dnd = None;

        for mon in self.monitors_mut() {
            mon.dnd_scroll_gesture_end();
        }

        for ws in self.workspaces_mut() {
            ws.dnd_scroll_gesture_end();
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(&window) {
                            return ws.interactive_resize_begin(window, edges);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(&window) {
                        return ws.interactive_resize_begin(window, edges);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return false;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            return ws.interactive_resize_update(window, delta);
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        return ws.interactive_resize_update(window, delta);
                    }
                }
            }
        }

        false
    }

    pub fn interactive_resize_end(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.interactive_resize_end(Some(window));
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.interactive_resize_end(Some(window));
                        return;
                    }
                }
            }
        }
    }

    pub fn move_workspace_down(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_workspace_down();
    }

    pub fn move_workspace_up(&mut self) {
        let Some(monitor) = self.active_monitor() else {
            return;
        };
        monitor.move_workspace_up();
    }

    pub fn move_workspace_to_idx(
        &mut self,
        reference: Option<(Option<Output>, usize)>,
        new_idx: usize,
    ) {
        let (monitor, old_idx) = if let Some((output, old_idx)) = reference {
            let monitor = if let Some(output) = output {
                let Some(monitor) = self.monitor_for_output_mut(&output) else {
                    return;
                };
                monitor
            } else {
                // In case a numbered workspace reference is used, assume the active monitor
                let Some(monitor) = self.active_monitor() else {
                    return;
                };
                monitor
            };

            (monitor, old_idx)
        } else {
            let Some(monitor) = self.active_monitor() else {
                return;
            };
            let index = monitor.active_workspace_idx;
            (monitor, index)
        };

        monitor.move_workspace_to_idx(old_idx, new_idx);
    }

    pub fn set_workspace_name(&mut self, name: String, reference: Option<WorkspaceReference>) {
        // ignore the request if the name is already used by another workspace
        if self.find_workspace_by_name(&name).is_some() {
            return;
        }

        let ws = if let Some(reference) = reference {
            self.find_workspace_by_ref(reference)
        } else {
            self.active_workspace_mut()
        };
        let Some(ws) = ws else {
            return;
        };

        ws.name.replace(name);

        let wsid = ws.id();

        // If `ws` was the last workspace on a monitor, an empty workspace needs to be
        // added after.

        if let MonitorSet::Normal {
            monitors,
            active_monitor_idx,
            ..
        } = &mut self.monitor_set
        {
            let monitor = &mut monitors[*active_monitor_idx];
            if monitor
                .workspaces
                .last()
                .is_some_and(|last| last.id() == wsid)
            {
                monitor.add_workspace_bottom();
            }
        }
    }

    pub fn unset_workspace_name(&mut self, reference: Option<WorkspaceReference>) {
        let ws = if let Some(reference) = reference {
            self.find_workspace_by_ref(reference)
        } else {
            self.active_workspace_mut()
        };
        let Some(ws) = ws else {
            return;
        };
        let id = ws.id();

        self.unname_workspace_by_id(id);
    }

    pub fn set_monitors_overview_state(&mut self) {
        let MonitorSet::Normal { monitors, .. } = &mut self.monitor_set else {
            return;
        };

        for mon in monitors {
            mon.overview_open = self.overview_open;
            if !self.overview_open {
                // The strip is gone, so a reorder drag on it has nothing left to drop
                // onto — it must not survive to be applied when the overview reopens.
                mon.cancel_thumb_drag();
            }
            mon.set_overview_progress(self.overview_progress.as_ref());
        }
    }

    pub fn toggle_overview(&mut self) {
        self.overview_open = !self.overview_open;

        if !self.overview_open {
            // Leaving: drop the picker overlay before the exit animation starts, rather than
            // easing it down. `render_expose` raises the hovered preview above its neighbours
            // for as long as its hover value is non-zero, and nothing else clears it on the way
            // out — the input handler only recomputes hover on pointer motion, which a
            // keyboard-driven exit never produces. The preview would fly home drawn on top of
            // windows that are really above it, then snap into its true place the instant the
            // overview handed off to the normal render path.
            for ws in self.workspaces_mut() {
                ws.clear_expose_hover();
            }
        }

        let from = self.overview_progress.take().map_or(0., |p| p.value());
        let to = if self.overview_open { 1. } else { 0. };

        self.overview_progress = Some(OverviewProgress::Animation(Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            self.options.animations.overview_open_close.0,
        )));

        self.set_monitors_overview_state();

        if self.overview_open {
            // Always enter in the window picker: the show-apps state froze at the
            // last close and is snapped back once hidden, but reset on the rising
            // edge too so a direct open never inherits a stale grid.
            self.app_grid_open = false;
            for mon in self.monitors_mut() {
                mon.reset_app_grid();
            }
        }
        // On close we deliberately do NOT touch the app-grid state — it freezes so
        // the zoom-out blends from the app-grid box, then snaps back once hidden
        // (see `advance_animations`).
    }

    /// Whether the overview's app grid (show-apps state) is showing.
    pub fn is_app_grid_open(&self) -> bool {
        self.overview_open && self.app_grid_open
    }

    /// Toggle the app grid (the show-apps button / its keybind). A no-op unless the
    /// overview is open. Returns whether the state changed.
    pub fn toggle_app_grid(&mut self) -> bool {
        if !self.overview_open {
            return false;
        }
        self.set_app_grid(!self.app_grid_open);
        true
    }

    /// Shift a state *up*: window picker → app grid (`overviewControls.js:669-676`,
    /// which clamps the target at `APP_GRID`, so this only ever opens). Returns
    /// whether the state changed.
    pub fn open_app_grid(&mut self) -> bool {
        if !self.overview_open || self.app_grid_open {
            return false;
        }
        self.set_app_grid(true);
        true
    }

    /// Ease the app grid back to the window picker if it is open — the grid tier of
    /// the overview's Escape (`searchController.js:153-159`: search → grid → hide).
    /// Returns whether it was open, so Escape can fall through to closing the
    /// overview when it wasn't.
    pub fn close_app_grid(&mut self) -> bool {
        if self.overview_open && self.app_grid_open {
            self.set_app_grid(false);
            true
        } else {
            false
        }
    }

    fn set_app_grid(&mut self, open: bool) {
        if open == self.app_grid_open {
            return;
        }
        self.app_grid_open = open;
        for mon in self.monitors_mut() {
            mon.set_app_grid(open);
        }
    }

    pub fn open_overview(&mut self) -> bool {
        if self.overview_open {
            return false;
        }

        self.toggle_overview();
        true
    }

    pub fn close_overview(&mut self) -> bool {
        if !self.overview_open {
            return false;
        }

        self.toggle_overview();
        true
    }

    pub fn toggle_overview_to_workspace(&mut self, ws_idx: usize) {
        let config = self.options.animations.overview_open_close.0;
        if let Some(mon) = self.active_monitor() {
            mon.activate_workspace_with_anim_config(ws_idx, Some(config));
        }
        self.toggle_overview();
    }

    pub fn start_open_animation_for_window(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if move_.tile.window().id() == window {
                return;
            }
        }

        for ws in self.workspaces_mut() {
            if ws.start_open_animation(window) {
                return;
            }
        }
    }

    pub fn store_unmap_snapshot(
        &mut self,
        renderer: SnapshotRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        window: &W::Id,
    ) {
        let _span = tracy_client::span!("Layout::store_unmap_snapshot");

        let zoom = self.interactive_move_zoom();

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let pos_within_output = move_.tile_render_location(zoom);

                // Computation matches update_render_elements().
                let view_rect =
                    Rectangle::new(pos_within_output.upscale(-1.), output_size(&move_.output))
                        .downscale(zoom);
                move_.tile.update_render_elements(false, view_rect);

                move_.tile.store_unmap_snapshot_if_empty(
                    renderer,
                    xray,
                    xray_has_blocked_out_layers,
                    XrayPos::new(pos_within_output, zoom),
                );
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    // Not the drag's zoom: this window is on a workspace, so the
                    // snapshot must record the zoom its *own* monitor renders at.
                    let ws_zoom = mon.overview_zoom();
                    for (ws, geo) in mon.workspaces_with_render_geo_mut(false) {
                        if ws.has_window(window) {
                            ws.store_unmap_snapshot_if_empty(
                                renderer,
                                xray,
                                xray_has_blocked_out_layers,
                                XrayPos::new(geo.loc, ws_zoom),
                                window,
                            );
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.store_unmap_snapshot_if_empty(
                            renderer,
                            xray,
                            xray_has_blocked_out_layers,
                            XrayPos::default(),
                            window,
                        );
                        return;
                    }
                }
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let _ = move_.tile.take_unmap_snapshot();
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.clear_unmap_snapshot(window);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.clear_unmap_snapshot(window);
                        return;
                    }
                }
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let _span = tracy_client::span!("Layout::start_close_animation_for_window");

        let zoom = self.interactive_move_zoom();

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if move_.tile.window().id() == window {
                let Some(snapshot) = move_.tile.take_unmap_snapshot() else {
                    return;
                };
                let tile_pos = move_.tile_render_location(zoom);
                let tile_size = move_.tile.tile_size();

                let output = move_.output.clone();
                let pointer_pos_within_output = move_.pointer_pos_within_output;
                let Some(mon) = self.monitor_for_output_mut(&output) else {
                    return;
                };
                let Some((ws, ws_geo)) = mon.workspace_under(pointer_pos_within_output) else {
                    return;
                };
                let ws_id = ws.id();
                let ws = mon
                    .workspaces
                    .iter_mut()
                    .find(|ws| ws.id() == ws_id)
                    .unwrap();

                let tile_pos = tile_pos - ws_geo.loc;
                ws.start_close_animation_for_tile(snapshot, tile_size, tile_pos, blocker);
                return;
            }
        }

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                for mon in monitors {
                    for ws in &mut mon.workspaces {
                        if ws.has_window(window) {
                            ws.start_close_animation_for_window(window, blocker);
                            return;
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    if ws.has_window(window) {
                        ws.start_close_animation_for_window(window, blocker);
                        return;
                    }
                }
            }
        }
    }

    /// The on-screen footprint of the window being dragged, if any: in the
    /// overview that is the picker preview it was picked up as, shrunk toward
    /// [`WINDOW_DND_SIZE`] as the drag gets going. Same geometry
    /// [`Self::render_interactive_move_for_output`] draws.
    pub fn interactive_move_drawn_size(&self) -> Option<Size<f64, Logical>> {
        let InteractiveMoveState::Moving(move_) = self.interactive_move.as_ref()? else {
            return None;
        };
        let zoom = self.overview_zoom_for_output(&move_.output);
        let scale = zoom * move_.expose_extra_scale(zoom);
        Some(move_.tile.tile_size().upscale(scale))
    }

    pub fn render_interactive_move_for_output(
        &self,
        ctx: RenderCtx,
        output: &Output,
        push: &mut dyn FnMut(RescaleRenderElement<TileRenderElement>),
    ) {
        if self.update_render_elements_time != self.clock.now() {
            error!("clock moved between updating render elements and rendering");
        }

        let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move else {
            return;
        };

        if &move_.output != output {
            return;
        }

        let scale = Scale::from(move_.output.current_scale().fractional_scale());
        let zoom = self.overview_zoom_for_output(output);
        // In the GNOME overview the drag carries the picker preview: the
        // tile keeps the on-screen footprint it was picked up at.
        let render_scale = zoom * move_.expose_extra_scale(zoom);
        let pos_in_backdrop = move_.tile_render_location(zoom);
        let xray_pos = XrayPos::new(pos_in_backdrop, render_scale);

        move_
            .tile
            .render(ctx, pos_in_backdrop, xray_pos, true, &mut |elem| {
                push(RescaleRenderElement::from_element(
                    elem,
                    pos_in_backdrop.to_physical_precise_round(scale),
                    render_scale,
                ));
            });
    }

    pub fn refresh(&mut self, is_active: bool) {
        let _span = tracy_client::span!("Layout::refresh");

        self.is_active = is_active;

        let mut ongoing_scrolling_dnd = self.dnd.is_some().then_some(true);

        if let Some(InteractiveMoveState::Moving(move_)) = &mut self.interactive_move {
            if !self.overview_open {
                // The overview closed mid-drag: drop the picker-preview
                // footprint and render the window at its real size.
                move_.expose_pickup_size = None;
                move_.expose_dnd_shrink = None;
            }

            let win = move_.tile.window_mut();

            win.set_active_in_column(true);
            win.set_floating(move_.is_floating);
            win.set_activated(true);

            win.set_interactive_resize(None);

            win.set_bounds(output_size(&move_.output).to_i32_round());

            win.send_pending_configure();
            win.refresh();

            ongoing_scrolling_dnd.get_or_insert(!move_.is_floating);
        } else if let Some(InteractiveMoveState::Starting { window_id, .. }) =
            &self.interactive_move
        {
            ongoing_scrolling_dnd.get_or_insert_with(|| {
                let (_, _, ws) = self
                    .workspaces()
                    .find(|(_, _, ws)| ws.has_window(window_id))
                    .unwrap();
                !ws.is_floating(window_id)
            });
        }

        match &mut self.monitor_set {
            MonitorSet::Normal {
                monitors,
                active_monitor_idx,
                ..
            } => {
                for (idx, mon) in monitors.iter_mut().enumerate() {
                    let is_active = self.is_active
                        && idx == *active_monitor_idx
                        && !matches!(self.interactive_move, Some(InteractiveMoveState::Moving(_)));

                    if ongoing_scrolling_dnd.is_some() && self.overview_open {
                        // Begin the scroll on new monitors and when opening the overview.
                        mon.dnd_scroll_gesture_begin();
                    } else if !self.overview_open {
                        mon.dnd_scroll_gesture_end();
                    }

                    for (ws_idx, ws) in mon.workspaces.iter_mut().enumerate() {
                        let is_focused = is_active && ws_idx == mon.active_workspace_idx;
                        ws.refresh(is_active, is_focused);

                        if let Some(is_scrolling) = ongoing_scrolling_dnd {
                            // Lock or unlock the view for scrolling interactive move.
                            if is_scrolling {
                                ws.dnd_scroll_gesture_begin();
                            } else {
                                ws.dnd_scroll_gesture_end();
                            }
                        } else {
                            // Cancel the view offset gesture after workspace switches, moves, etc.
                            if !self.overview_open && ws_idx != mon.active_workspace_idx {
                                ws.view_offset_gesture_end(None);
                            }
                        }
                    }
                }
            }
            MonitorSet::NoOutputs { workspaces, .. } => {
                for ws in workspaces {
                    ws.refresh(false, false);
                    ws.view_offset_gesture_end(None);
                }
            }
        }
    }

    /// See [`Workspace::seed_unmaximize_geometry`]. `output_origin` converts the stored global
    /// rect into the workspace's frame.
    pub fn seed_unmaximize_geometry(
        &mut self,
        id: &W::Id,
        rect: Rectangle<f64, Logical>,
        output_origin: Point<f64, Logical>,
    ) {
        for ws in self.workspaces_mut() {
            if ws.seed_unmaximize_geometry(id, rect, output_origin) {
                return;
            }
        }
    }

    /// Everything the session store needs about `id`, other than the output origin.
    ///
    /// The workspace index is **per monitor**, while the rect the caller composes is global. The
    /// pair is deliberate: restore resolves the output from the rect first, then indexes into that
    /// monitor's workspaces.
    pub fn session_snapshot(&self, id: &W::Id) -> Option<SessionSnapshot<'_>> {
        self.workspaces().find_map(|(monitor, idx, ws)| {
            let (sizing_mode, floating_rect) = ws.session_snapshot(id)?;
            Some(SessionSnapshot {
                output: monitor.map(|mon| &mon.output),
                workspace_idx: idx,
                sizing_mode,
                floating_rect,
            })
        })
    }

    pub fn workspaces(
        &self,
    ) -> impl Iterator<Item = (Option<&Monitor<W>>, usize, &Workspace<W>)> + '_ {
        let iter_normal;
        let iter_no_outputs;

        match &self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors.iter().flat_map(|mon| {
                    mon.workspaces
                        .iter()
                        .enumerate()
                        .map(move |(idx, ws)| (Some(mon), idx, ws))
                });

                iter_normal = Some(it);
                iter_no_outputs = None;
            }
            MonitorSet::NoOutputs { workspaces } => {
                let it = workspaces
                    .iter()
                    .enumerate()
                    .map(|(idx, ws)| (None, idx, ws));

                iter_normal = None;
                iter_no_outputs = Some(it);
            }
        }

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    pub fn workspaces_mut(&mut self) -> impl Iterator<Item = &mut Workspace<W>> + '_ {
        let iter_normal;
        let iter_no_outputs;

        match &mut self.monitor_set {
            MonitorSet::Normal { monitors, .. } => {
                let it = monitors
                    .iter_mut()
                    .flat_map(|mon| mon.workspaces.iter_mut());

                iter_normal = Some(it);
                iter_no_outputs = None;
            }
            MonitorSet::NoOutputs { workspaces } => {
                let it = workspaces.iter_mut();

                iter_normal = None;
                iter_no_outputs = Some(it);
            }
        }

        let iter_normal = iter_normal.into_iter().flatten();
        let iter_no_outputs = iter_no_outputs.into_iter().flatten();
        iter_normal.chain(iter_no_outputs)
    }

    pub fn windows(&self) -> impl Iterator<Item = (Option<&Monitor<W>>, &W)> {
        let moving_window = self
            .interactive_move
            .as_ref()
            .and_then(|x| x.moving())
            .map(|move_| (self.monitor_for_output(&move_.output), move_.tile.window()))
            .into_iter();

        let rest = self
            .workspaces()
            .flat_map(|(mon, _, ws)| ws.windows().map(move |win| (mon, win)));

        moving_window.chain(rest)
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|(_, win)| win.id() == window)
    }

    pub fn is_overview_open(&self) -> bool {
        self.overview_open
    }

    /// The workspace a drop at `pos` on `output` would land on, if it is an
    /// existing one: the workspace under the pointer in the picker, or the
    /// thumbnail under it in the strip (gnome-shell's `Workspace.acceptDrop` /
    /// `WorkspaceThumbnail.acceptDrop`). A drop into a gap between thumbnails —
    /// which for a *window* drag inserts a new workspace — has no workspace to
    /// name, so it answers `None`.
    pub fn drop_workspace_at(
        &self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<WorkspaceId> {
        let mon = self.monitor_for_output(output)?;
        match mon.insert_position(pos).0 {
            InsertWorkspace::Existing(id) => Some(id),
            InsertWorkspace::NewAt(_) => None,
        }
    }

    /// Point the overview's window picker at the preview under the pointer, or at
    /// nothing. Hovering a preview grows it and raises it above its neighbours
    /// (gnome-shell's `showOverlay`, `windowPreview.js:310`). Returns whether
    /// anything changed, so the caller can queue a redraw.
    pub fn set_expose_hover(&mut self, window: Option<&W::Id>) -> bool {
        let mut changed = false;
        for ws in self.workspaces_mut() {
            changed |= ws.set_expose_hover(window);
        }
        changed
    }

    /// Whether the overview is still animating open — see
    /// [`Monitor::is_overview_opening`]. Asked of the active monitor, the one the
    /// overlay key is about to act on.
    pub fn is_overview_opening(&self) -> bool {
        self.active_monitor_ref()
            .is_some_and(Monitor::is_overview_opening)
    }

    /// Whether the session is in GNOME (floating) windowing mode, where the top
    /// panel is drawn and reserves a strut.
    pub fn is_gnome_mode(&self) -> bool {
        self.options.layout.windowing_mode == WindowingMode::Floating
    }

    /// Where a window's preview actually draws on screen right now, in output
    /// coordinates: its picker slot plus the hover overlay's growth. Same
    /// geometry the picker renders (`Workspace::expose_drawn_rect`).
    pub fn expose_drawn_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        self.monitors().find_map(|mon| {
            let progress = mon.expose_progress()?;
            let zoom = mon.overview_zoom();
            let (ws, geo) = mon
                .workspaces_with_render_geo()
                .find(|(ws, _)| ws.has_window(window))?;
            let rect = ws.expose_drawn_rect(window, progress, zoom)?;
            Some(Rectangle::new(
                geo.loc + rect.loc.upscale(zoom),
                rect.size.upscale(zoom),
            ))
        })
    }

    /// The overview picker slot of a window, in output coordinates — where
    /// the window's preview sits on screen in the GNOME overview.
    pub fn expose_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        self.monitors().find_map(|mon| {
            mon.expose_progress()?;
            let zoom = mon.overview_zoom();
            let (ws, geo) = mon
                .workspaces_with_render_geo()
                .find(|(ws, _)| ws.has_window(window))?;
            let slot = ws.expose_slot(window)?;
            Some(Rectangle::new(
                geo.loc + slot.loc.upscale(zoom),
                slot.size.upscale(zoom),
            ))
        })
    }
}

impl<W: LayoutElement> Default for MonitorSet<W> {
    fn default() -> Self {
        Self::NoOutputs { workspaces: vec![] }
    }
}

/// Interpolates the workspace zoom from 1 (overview closed) to `zoom` (fully
/// zoomed out) along the overview progress. What `zoom` itself is depends on
/// the mode — see [`Monitor::zoom_at`].
fn compute_overview_zoom(zoom: f64, overview_progress: Option<f64>) -> f64 {
    if let Some(p) = overview_progress {
        (1. - p * (1. - zoom)).max(0.0001)
    } else {
        1.
    }
}
