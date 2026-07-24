use std::cmp::min;
use std::iter::zip;
use std::rc::Rc;
use std::time::Duration;

use niri_config::{CornerRadius, LayoutPart, WindowingMode};
use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};

use super::focus_ring::FocusRing;
use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::scrolling::{Column, ColumnWidth};
use super::thumbnails::{self, Strip};
use super::tile::Tile;
use super::workspace::{
    compute_working_area, OutputId, Workspace, WorkspaceAddWindowTarget, WorkspaceId,
    WorkspaceRenderElement,
};
use super::{compute_overview_zoom, ActivateWindow, HitType, LayoutElement, Options};
use crate::animation::{Animation, Clock};
use crate::gnome::EdgeTileTarget;
use crate::input::swipe_tracker::SwipeTracker;
use crate::niri_render_elements;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::SolidColorRenderElement;
use crate::render_helpers::vulkan::VkTexture;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::rubber_band::RubberBand;
use crate::ui::overview_layout::{self, ControlsLayout};
use crate::utils::transaction::Transaction;
use crate::utils::{
    output_size, round_logical_in_physical, round_logical_in_physical_max1, ResizeEdge,
};
use crate::wallpaper::Wallpaper;

/// Amount of touchpad movement to scroll the height of one workspace.
const WORKSPACE_GESTURE_MOVEMENT: f64 = 300.;

const WORKSPACE_GESTURE_RUBBER_BAND: RubberBand = RubberBand {
    stiffness: 0.5,
    limit: 0.05,
};

/// Amount of DnD edge scrolling to scroll the height of one workspace.
///
/// This constant is tied to the default dnd-edge-workspace-switch max-speed setting.
const WORKSPACE_DND_EDGE_SCROLL_MOVEMENT: f64 = 1500.;

/// Grace period between DnD edge-scroll snaps in GNOME mode.
///
/// GNOME has no counterpart for this affordance; 750 ms is its usual
/// "the pointer is still interacting, hold off" delay (gnome-shell's
/// WINDOW_REPOSITIONING_DELAY).
const WORKSPACE_DND_EDGE_SNAP_GRACE: Duration = Duration::from_millis(750);

#[derive(Debug)]
pub struct Monitor<W: LayoutElement> {
    /// Output for this monitor.
    pub(super) output: Output,
    /// Cached name of the output.
    output_name: String,
    /// Latest known scale for this output.
    scale: smithay::output::Scale,
    /// Latest known size for this output.
    view_size: Size<f64, Logical>,
    /// Latest known working area for this output.
    ///
    /// Not rounded to physical pixels.
    // FIXME: since this is used for things like DnD scrolling edges in the overview, ideally this
    // should only consider overlay and top layer-shell surfaces. However, Smithay doesn't easily
    // let you do this at the moment.
    working_area: Rectangle<f64, Logical>,
    // Must always contain at least one.
    pub(super) workspaces: Vec<Workspace<W>>,
    /// Index of the currently active workspace.
    pub(super) active_workspace_idx: usize,
    /// ID of the previously active workspace.
    pub(super) previous_workspace_id: Option<WorkspaceId>,
    /// In-progress switch between workspaces.
    pub(super) workspace_switch: Option<WorkspaceSwitch>,
    /// Indication where an interactively-moved window is about to be placed.
    pub(super) insert_hint: Option<InsertHint>,
    /// Insert hint element for rendering.
    insert_hint_element: InsertHintElement,
    /// Location to render the insert hint element.
    insert_hint_render_loc: Option<InsertHintRenderLoc>,
    /// The active-workspace ring on the overview thumbnails strip
    /// (gnome-shell's `.workspace-thumbnail-indicator`).
    thumb_indicator: FocusRing,
    /// The strip's new-workspace drop placeholder pill (gnome-shell's
    /// `.placeholder`).
    thumb_placeholder: FocusRing,
    /// Whether the overview is open.
    pub(super) overview_open: bool,
    /// Progress of the overview zoom animation, 1 is fully in overview.
    overview_progress: Option<OverviewProgress>,
    /// gnome-shell's `ThumbnailsBox.expandFraction` (`overviewControls.js:358-366`):
    /// eased 0↔1 when the strip's `should-show` flips, so the picker box grows
    /// into the thumbnails band (and back) instead of jumping.
    thumbnails_expand: Option<Animation>,
    /// The settled target of [`Self::thumbnails_expand`], for edge detection.
    thumbnails_shown: bool,
    /// Clock for driving animations.
    pub(super) clock: Clock,
    /// Configurable properties of the layout as received from the parent layout.
    pub(super) base_options: Rc<Options>,
    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
    /// Layout config overrides for this monitor.
    layout_config: Option<niri_config::LayoutPart>,
}

#[derive(Debug)]
pub enum WorkspaceSwitch {
    Animation(Animation),
    Gesture(WorkspaceSwitchGesture),
}

#[derive(Debug)]
pub struct WorkspaceSwitchGesture {
    /// Index of the workspace where the gesture was started.
    center_idx: usize,
    /// Fractional workspace index where the gesture was started.
    ///
    /// Can differ from center_idx when starting a gesture in the middle between workspaces, for
    /// example by "catching" an animation.
    start_idx: f64,
    /// Current, fractional workspace index.
    pub(super) current_idx: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, if there's a workspace switch during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,
    /// Whether the gesture is clamped to +-1 workspace around the center.
    is_clamped: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
    // When the last GNOME-mode DnD edge snap switched workspaces; the next
    // snap waits out [`WORKSPACE_DND_EDGE_SNAP_GRACE`] from here.
    dnd_snap_last_switch: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertPosition {
    NewColumn(usize),
    InColumn(usize, usize),
    Floating,
    /// Floating, but dropped on a screen edge: tile or maximize (GNOME).
    EdgeTile(EdgeTileTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InsertWorkspace {
    Existing(WorkspaceId),
    NewAt(usize),
}

#[derive(Debug)]
pub(super) struct InsertHint {
    pub workspace: InsertWorkspace,
    pub position: InsertPosition,
    pub corner_radius: CornerRadius,
    /// Whether the hover is on the thumbnails strip (renders the strip's
    /// drop placeholder rather than the between-workspaces bar).
    pub via_strip: bool,
}

#[derive(Debug, Clone, Copy)]
struct InsertHintRenderLoc {
    workspace: InsertWorkspace,
    location: Point<f64, Logical>,
}

#[derive(Debug)]
pub(super) enum OverviewProgress {
    Animation(Animation),
    Value(f64),
}

/// Where to put a newly added window.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum MonitorAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// On this workspace.
    Workspace {
        /// Id of the target workspace.
        id: WorkspaceId,
        /// Override where the window will open as a new column.
        column_idx: Option<usize>,
    },
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl<'a, W: LayoutElement> Copy for MonitorAddWindowTarget<'a, W> {}

impl<'a, W: LayoutElement> Clone for MonitorAddWindowTarget<'a, W> {
    fn clone(&self) -> Self {
        *self
    }
}

niri_render_elements! {
    MonitorInnerRenderElement => {
        Workspace = CropRenderElement<WorkspaceRenderElement>,
        InsertHint = CropRenderElement<InsertHintRenderElement>,
        // Insert hint between workspaces, and the thumbnail-strip indicator.
        Ring = InsertHintRenderElement,
        Shadow = ShadowRenderElement,
        SolidColor = SolidColorRenderElement,
        // The wallpaper in a workspace thumbnail.
        RoundedTexture = RoundedTextureRenderElement<VkTexture>,
    }
}

pub type MonitorRenderElement =
    RelocateRenderElement<RescaleRenderElement<MonitorInnerRenderElement>>;

impl WorkspaceSwitch {
    pub fn current_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.value(),
            WorkspaceSwitch::Gesture(gesture) => {
                gesture.current_idx + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    pub fn target_idx(&self) -> f64 {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.to(),
            WorkspaceSwitch::Gesture(gesture) => gesture.current_idx,
        }
    }

    pub fn offset(&mut self, delta: isize) {
        match self {
            WorkspaceSwitch::Animation(anim) => anim.offset(delta as f64),
            WorkspaceSwitch::Gesture(gesture) => {
                if delta >= 0 {
                    gesture.center_idx += delta as usize;
                } else {
                    gesture.center_idx -= (-delta) as usize;
                }
                gesture.start_idx += delta as f64;
                gesture.current_idx += delta as f64;
            }
        }
    }

    fn is_animation_ongoing(&self) -> bool {
        match self {
            WorkspaceSwitch::Animation(_) => true,
            // A DnD scroll with the pointer in the trigger zone
            // (dnd_nonzero_start_time) counts as ongoing: its delay and
            // snap-grace timers are evaluated frame by frame, so frames must
            // keep coming while the pointer sits still on the edge.
            WorkspaceSwitch::Gesture(gesture) => {
                gesture.animation.is_some() || gesture.dnd_nonzero_start_time.is_some()
            }
        }
    }
}

impl WorkspaceSwitchGesture {
    fn min_max(&self, workspace_count: usize) -> (f64, f64) {
        if self.is_clamped {
            let min = self.center_idx.saturating_sub(1) as f64;
            let max = (self.center_idx + 1).min(workspace_count - 1) as f64;
            (min, max)
        } else {
            (0., (workspace_count - 1) as f64)
        }
    }

    fn animate_from(&mut self, from: f64, clock: Clock, config: niri_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl InsertWorkspace {
    fn existing_id(self) -> Option<WorkspaceId> {
        match self {
            InsertWorkspace::Existing(id) => Some(id),
            InsertWorkspace::NewAt(_) => None,
        }
    }
}

impl OverviewProgress {
    pub fn value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.value(),
            OverviewProgress::Value(v) => *v,
        }
    }

    pub fn clamped_value(&self) -> f64 {
        match self {
            OverviewProgress::Animation(anim) => anim.clamped_value(),
            OverviewProgress::Value(v) => *v,
        }
    }
}

impl From<&super::OverviewProgress> for OverviewProgress {
    fn from(value: &super::OverviewProgress) -> Self {
        match value {
            super::OverviewProgress::Animation(anim) => Self::Animation(anim.clone()),
            super::OverviewProgress::Gesture(gesture) => Self::Value(gesture.value),
            super::OverviewProgress::Open => Self::Value(1.),
        }
    }
}

impl<W: LayoutElement> Monitor<W> {
    pub fn new(
        output: Output,
        mut workspaces: Vec<Workspace<W>>,
        ws_id_to_activate: Option<WorkspaceId>,
        clock: Clock,
        base_options: Rc<Options>,
        layout_config: Option<LayoutPart>,
    ) -> Self {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(layout_config.as_ref()));

        let scale = output.current_scale();
        let view_size = output_size(&output);
        let working_area = compute_working_area(&output, &options);

        // Prepare the workspaces: set output, empty first, empty last.
        let mut active_workspace_idx = 0;

        for (idx, ws) in workspaces.iter_mut().enumerate() {
            assert!(ws.has_windows_or_name());

            ws.set_output(Some(output.clone()));
            ws.update_config(options.clone());

            if ws_id_to_activate.is_some_and(|id| ws.id() == id) {
                active_workspace_idx = idx;
            }
        }

        if options.layout.empty_workspace_above_first && !workspaces.is_empty() {
            let ws = Workspace::new(output.clone(), clock.clone(), options.clone());
            workspaces.insert(0, ws);
            active_workspace_idx += 1;
        }

        let ws = Workspace::new(output.clone(), clock.clone(), options.clone());
        workspaces.push(ws);
        let workspaces_len = workspaces.len();

        Self {
            output_name: output.name(),
            output,
            scale,
            view_size,
            working_area,
            workspaces,
            active_workspace_idx,
            previous_workspace_id: None,
            insert_hint: None,
            insert_hint_element: InsertHintElement::new(options.layout.insert_hint),
            thumb_indicator: FocusRing::new(thumbnail_indicator_config(crate::gnome::ACCENT_BLUE)),
            thumb_placeholder: FocusRing::new(thumbnail_placeholder_config()),
            insert_hint_render_loc: None,
            overview_open: false,
            overview_progress: None,
            thumbnails_expand: None,
            thumbnails_shown: workspaces_len > thumbnails::NUM_WORKSPACES_THRESHOLD,
            workspace_switch: None,
            clock,
            base_options,
            options,
            layout_config,
        }
    }

    pub fn into_workspaces(mut self) -> Vec<Workspace<W>> {
        self.workspaces.retain(|ws| ws.has_windows_or_name());

        for ws in &mut self.workspaces {
            ws.set_output(None);
        }

        self.workspaces
    }

    pub fn output(&self) -> &Output {
        &self.output
    }

    pub fn output_name(&self) -> &String {
        &self.output_name
    }

    pub fn active_workspace_idx(&self) -> usize {
        self.active_workspace_idx
    }

    /// Number of workspaces on this monitor (including the trailing empty one). Drives the panel's
    /// workspace-dot indicator count (GNOME's `WorkspacesAdjustment.upper`).
    pub fn n_workspaces(&self) -> usize {
        self.workspaces.len()
    }

    pub fn active_workspace_ref(&self) -> &Workspace<W> {
        &self.workspaces[self.active_workspace_idx]
    }

    pub fn find_named_workspace(&self, workspace_name: &str) -> Option<&Workspace<W>> {
        self.workspaces.iter().find(|ws| {
            ws.name
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn find_named_workspace_index(&self, workspace_name: &str) -> Option<usize> {
        self.workspaces.iter().position(|ws| {
            ws.name
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(workspace_name))
        })
    }

    pub fn active_workspace(&mut self) -> &mut Workspace<W> {
        &mut self.workspaces[self.active_workspace_idx]
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> {
        self.workspaces.iter().flat_map(|ws| ws.windows())
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn add_workspace_at(&mut self, idx: usize) {
        let ws = Workspace::new(
            self.output.clone(),
            self.clock.clone(),
            self.options.clone(),
        );

        self.workspaces.insert(idx, ws);
        if idx <= self.active_workspace_idx {
            self.active_workspace_idx += 1;
        }

        if let Some(switch) = &mut self.workspace_switch {
            if idx as f64 <= switch.target_idx() {
                switch.offset(1);
            }
        }
    }

    pub fn add_workspace_top(&mut self) {
        self.add_workspace_at(0);
    }

    pub fn add_workspace_bottom(&mut self) {
        self.add_workspace_at(self.workspaces.len());
    }

    pub fn activate_workspace(&mut self, idx: usize) {
        self.activate_workspace_with_anim_config(idx, None);
    }

    pub fn activate_workspace_with_anim_config(
        &mut self,
        idx: usize,
        config: Option<niri_config::Animation>,
    ) {
        // FIXME: also compute and use current velocity.
        let current_idx = self.workspace_render_idx();

        if self.active_workspace_idx != idx {
            self.previous_workspace_id = Some(self.workspaces[self.active_workspace_idx].id());
        }

        let prev_active_idx = self.active_workspace_idx;
        self.active_workspace_idx = idx;

        let config = config.unwrap_or(self.options.animations.workspace_switch.0);

        match &mut self.workspace_switch {
            // During a DnD scroll, we want to visually animate even if idx matches the active idx.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                gesture.center_idx = idx;

                // Adjust start_idx to make current_idx point at idx.
                let current_pos = gesture.current_idx - gesture.start_idx;
                gesture.start_idx = idx as f64 - current_pos;
                let prev_current_idx = gesture.current_idx;
                gesture.current_idx = idx as f64;

                let current_idx_delta = gesture.current_idx - prev_current_idx;
                gesture.animate_from(-current_idx_delta, self.clock.clone(), config);
            }
            _ => {
                // Don't animate if nothing changed.
                if prev_active_idx == idx {
                    return;
                }

                self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
                    self.clock.clone(),
                    current_idx,
                    idx as f64,
                    0.,
                    config,
                )));
            }
        }
    }

    pub(super) fn resolve_add_window_target<'a>(
        &mut self,
        target: MonitorAddWindowTarget<'a, W>,
    ) -> (usize, WorkspaceAddWindowTarget<'a, W>) {
        match target {
            MonitorAddWindowTarget::Auto => {
                (self.active_workspace_idx, WorkspaceAddWindowTarget::Auto)
            }
            MonitorAddWindowTarget::Workspace { id, column_idx } => {
                let idx = self.workspaces.iter().position(|ws| ws.id() == id).unwrap();
                let target = if let Some(column_idx) = column_idx {
                    WorkspaceAddWindowTarget::NewColumnAt(column_idx)
                } else {
                    WorkspaceAddWindowTarget::Auto
                };
                (idx, target)
            }
            MonitorAddWindowTarget::NextTo(win_id) => {
                let idx = self
                    .workspaces
                    .iter_mut()
                    .position(|ws| ws.has_window(win_id))
                    .unwrap();
                (idx, WorkspaceAddWindowTarget::NextTo(win_id))
            }
        }
    }

    pub fn add_window(
        &mut self,
        window: W,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        // Currently, everything a workspace sets on a Tile is the same across all workspaces of a
        // monitor. So we can use any workspace, not necessarily the exact target workspace.
        let tile = self.workspaces[0].make_tile(window);

        self.add_tile(
            tile,
            target,
            activate,
            true,
            width,
            is_full_width,
            is_floating,
        );
    }

    pub fn add_column(&mut self, mut workspace_idx: usize, column: Column<W>, activate: bool) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_column(column, activate);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        if workspace_idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.layout.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }

        if activate {
            self.activate_workspace(workspace_idx);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_tile(
        &mut self,
        tile: Tile<W>,
        target: MonitorAddWindowTarget<W>,
        activate: ActivateWindow,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        let (mut workspace_idx, target) = self.resolve_add_window_target(target);

        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_tile(tile, target, activate, width, is_full_width, is_floating);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        if workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && workspace_idx == 0 {
            self.add_workspace_top();
            workspace_idx += 1;
        }

        if allow_to_activate_workspace && activate.map_smart(|| false) {
            self.activate_workspace(workspace_idx);
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        workspace_idx: usize,
        column_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        // FIXME: Refactor ActivateWindow enum to make this better.
        allow_to_activate_workspace: bool,
    ) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_tile_to_column(column_idx, tile_idx, tile, activate);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        // Since we're adding window to an existing column, the workspace isn't empty, and
        // therefore cannot be the last one, so we never need to insert a new empty workspace.

        if allow_to_activate_workspace && activate {
            self.activate_workspace(workspace_idx);
        }
    }

    pub fn clean_up_workspaces(&mut self) {
        assert!(self.workspace_switch.is_none());

        let range_start = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        for idx in (range_start..self.workspaces.len() - 1).rev() {
            if self.active_workspace_idx == idx {
                continue;
            }

            if !self.workspaces[idx].has_windows_or_name() {
                self.workspaces.remove(idx);
                if self.active_workspace_idx > idx {
                    self.active_workspace_idx -= 1;
                }
            }
        }

        // Special case handling when empty_workspace_above_first is set and all workspaces
        // are empty.
        if self.options.layout.empty_workspace_above_first && self.workspaces.len() == 2 {
            assert!(!self.workspaces[0].has_windows_or_name());
            assert!(!self.workspaces[1].has_windows_or_name());
            self.workspaces.remove(1);
            self.active_workspace_idx = 0;
        }
    }

    pub fn unname_workspace(&mut self, id: WorkspaceId) -> bool {
        let Some(ws) = self.workspaces.iter_mut().find(|ws| ws.id() == id) else {
            return false;
        };

        ws.unname();

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces();
        }

        true
    }

    pub fn remove_workspace_by_idx(&mut self, mut idx: usize) -> Workspace<W> {
        if idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
        }
        if self.options.layout.empty_workspace_above_first && idx == 0 {
            self.add_workspace_top();
            idx += 1;
        }

        let mut ws = self.workspaces.remove(idx);
        ws.set_output(None);

        // For monitor current workspace removal, we focus previous rather than next (<= rather
        // than <). This is different from columns and tiles, but it lets move-workspace-to-monitor
        // back and forth to preserve position.
        if idx <= self.active_workspace_idx && self.active_workspace_idx > 0 {
            self.active_workspace_idx -= 1;
        }

        self.workspace_switch = None;
        self.clean_up_workspaces();

        ws
    }

    pub fn insert_workspace(&mut self, mut ws: Workspace<W>, mut idx: usize, activate: bool) {
        ws.set_output(Some(self.output.clone()));
        ws.update_config(self.options.clone());

        // Don't insert past the last empty workspace.
        if idx == self.workspaces.len() {
            idx -= 1;
        }
        if idx == 0 && self.options.layout.empty_workspace_above_first {
            // Insert a new empty workspace on top to prepare for insertion of new workspace.
            self.add_workspace_top();
            idx += 1;
        }

        self.workspaces.insert(idx, ws);

        if idx <= self.active_workspace_idx {
            self.active_workspace_idx += 1;
        }

        if activate {
            self.workspace_switch = None;
            self.activate_workspace(idx);
        }

        self.workspace_switch = None;
        self.clean_up_workspaces();
    }

    pub fn append_workspaces(&mut self, mut workspaces: Vec<Workspace<W>>) {
        if workspaces.is_empty() {
            return;
        }

        for ws in &mut workspaces {
            ws.set_output(Some(self.output.clone()));
            ws.update_config(self.options.clone());
        }

        let empty_was_focused = self.active_workspace_idx == self.workspaces.len() - 1;

        // Push the workspaces from the removed monitor in the end, right before the
        // last, empty, workspace.
        let empty = self.workspaces.remove(self.workspaces.len() - 1);
        self.workspaces.extend(workspaces);
        self.workspaces.push(empty);

        // If empty_workspace_above_first is set and the first workspace is now no longer empty,
        // add a new empty workspace on top.
        if self.options.layout.empty_workspace_above_first
            && self.workspaces[0].has_windows_or_name()
        {
            self.add_workspace_top();
        }

        // If the empty workspace was focused on the primary monitor, keep it focused.
        if empty_was_focused {
            self.active_workspace_idx = self.workspaces.len() - 1;
        }

        // FIXME: if we're adding workspaces to currently invisible positions
        // (outside the workspace switch), we don't need to cancel it.
        self.workspace_switch = None;
        self.clean_up_workspaces();
    }

    pub fn move_down_or_to_workspace_down(&mut self) {
        if !self.active_workspace().move_down() {
            self.move_to_workspace_down(true);
        }
    }

    pub fn move_up_or_to_workspace_up(&mut self) {
        if !self.active_workspace().move_up() {
            self.move_to_workspace_up(true);
        }
    }

    pub fn focus_window_or_workspace_down(&mut self) {
        if !self.active_workspace().focus_down() {
            self.switch_workspace_down();
        }
    }

    pub fn focus_window_or_workspace_up(&mut self) {
        if !self.active_workspace().focus_up() {
            self.switch_workspace_up();
        }
    }

    pub fn move_to_workspace_up(&mut self, focus: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = source_workspace_idx.saturating_sub(1);
        if new_idx == source_workspace_idx {
            return;
        }
        let new_id = self.workspaces[new_idx].id();

        let workspace = &mut self.workspaces[source_workspace_idx];
        let Some(removed) = workspace.remove_active_tile(Transaction::new()) else {
            return;
        };

        let activate = if focus {
            ActivateWindow::Yes
        } else {
            ActivateWindow::Smart
        };

        self.add_tile(
            removed.tile,
            MonitorAddWindowTarget::Workspace {
                id: new_id,
                column_idx: None,
            },
            activate,
            true,
            removed.width,
            removed.is_full_width,
            removed.is_floating,
        );
    }

    pub fn move_to_workspace_down(&mut self, focus: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = min(source_workspace_idx + 1, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }
        let new_id = self.workspaces[new_idx].id();

        let workspace = &mut self.workspaces[source_workspace_idx];
        let Some(removed) = workspace.remove_active_tile(Transaction::new()) else {
            return;
        };

        let activate = if focus {
            ActivateWindow::Yes
        } else {
            ActivateWindow::Smart
        };

        self.add_tile(
            removed.tile,
            MonitorAddWindowTarget::Workspace {
                id: new_id,
                column_idx: None,
            },
            activate,
            true,
            removed.width,
            removed.is_full_width,
            removed.is_floating,
        );
    }

    pub fn move_to_workspace(
        &mut self,
        window: Option<&W::Id>,
        idx: usize,
        activate: ActivateWindow,
    ) {
        let source_workspace_idx = if let Some(window) = window {
            self.workspaces
                .iter()
                .position(|ws| ws.has_window(window))
                .unwrap()
        } else {
            self.active_workspace_idx
        };

        let new_idx = min(idx, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }
        let new_id = self.workspaces[new_idx].id();

        let activate = activate.map_smart(|| {
            window.is_none_or(|win| self.active_window().map(|win| win.id()) == Some(win))
        });

        let workspace = &mut self.workspaces[source_workspace_idx];
        let transaction = Transaction::new();
        let removed = if let Some(window) = window {
            workspace.remove_tile(window, transaction)
        } else if let Some(removed) = workspace.remove_active_tile(transaction) {
            removed
        } else {
            return;
        };

        self.add_tile(
            removed.tile,
            MonitorAddWindowTarget::Workspace {
                id: new_id,
                column_idx: None,
            },
            if activate {
                ActivateWindow::Yes
            } else {
                ActivateWindow::No
            },
            true,
            removed.width,
            removed.is_full_width,
            removed.is_floating,
        );

        if self.workspace_switch.is_none() {
            self.clean_up_workspaces();
        }
    }

    pub fn move_column_to_workspace_up(&mut self, activate: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = source_workspace_idx.saturating_sub(1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut self.workspaces[source_workspace_idx];
        if workspace.floating_is_active() {
            self.move_to_workspace_up(activate);
            return;
        }

        let Some(column) = workspace.remove_active_column() else {
            return;
        };

        self.add_column(new_idx, column, activate);
    }

    pub fn move_column_to_workspace_down(&mut self, activate: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = min(source_workspace_idx + 1, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut self.workspaces[source_workspace_idx];
        if workspace.floating_is_active() {
            self.move_to_workspace_down(activate);
            return;
        }

        let Some(column) = workspace.remove_active_column() else {
            return;
        };

        self.add_column(new_idx, column, activate);
    }

    pub fn move_column_to_workspace(&mut self, idx: usize, activate: bool) {
        let source_workspace_idx = self.active_workspace_idx;

        let new_idx = min(idx, self.workspaces.len() - 1);
        if new_idx == source_workspace_idx {
            return;
        }

        let workspace = &mut self.workspaces[source_workspace_idx];
        if workspace.floating_is_active() {
            let activate = if activate {
                ActivateWindow::Smart
            } else {
                ActivateWindow::No
            };
            self.move_to_workspace(None, idx, activate);
            return;
        }

        let Some(column) = workspace.remove_active_column() else {
            return;
        };

        self.add_column(new_idx, column, activate);
    }

    pub fn switch_workspace_up(&mut self) {
        let new_idx = match &self.workspace_switch {
            // During a DnD scroll, select the prev apparent workspace.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                let current = gesture.current_idx;
                let new = current.ceil() - 1.;
                new.clamp(0., (self.workspaces.len() - 1) as f64) as usize
            }
            _ => self.active_workspace_idx.saturating_sub(1),
        };

        self.activate_workspace(new_idx);
    }

    pub fn switch_workspace_down(&mut self) {
        let new_idx = match &self.workspace_switch {
            // During a DnD scroll, select the next apparent workspace.
            Some(WorkspaceSwitch::Gesture(gesture)) if gesture.dnd_last_event_time.is_some() => {
                let current = gesture.current_idx;
                let new = current.floor() + 1.;
                new.clamp(0., (self.workspaces.len() - 1) as f64) as usize
            }
            _ => min(self.active_workspace_idx + 1, self.workspaces.len() - 1),
        };

        self.activate_workspace(new_idx);
    }

    fn previous_workspace_idx(&self) -> Option<usize> {
        let id = self.previous_workspace_id?;
        self.workspaces.iter().position(|w| w.id() == id)
    }

    pub fn switch_workspace(&mut self, idx: usize) {
        self.activate_workspace(min(idx, self.workspaces.len() - 1));
    }

    pub fn switch_workspace_auto_back_and_forth(&mut self, idx: usize) {
        let idx = min(idx, self.workspaces.len() - 1);

        if idx == self.active_workspace_idx {
            if let Some(prev_idx) = self.previous_workspace_idx() {
                self.switch_workspace(prev_idx);
            }
        } else {
            self.switch_workspace(idx);
        }
    }

    pub fn switch_workspace_previous(&mut self) {
        if let Some(idx) = self.previous_workspace_idx() {
            self.switch_workspace(idx);
        }
    }

    pub fn active_window(&self) -> Option<&W> {
        self.active_workspace_ref().active_window()
    }

    pub fn advance_animations(&mut self) {
        match &mut self.workspace_switch {
            Some(WorkspaceSwitch::Animation(anim)) => {
                if anim.is_done() {
                    self.workspace_switch = None;
                    self.clean_up_workspaces();
                }
            }
            Some(WorkspaceSwitch::Gesture(gesture)) => {
                // Make sure the last event time doesn't go too much out of date (for
                // monitors not under cursor), causing sudden jumps.
                //
                // This happens after any dnd_scroll_gesture_scroll() calls (in
                // Layout::advance_animations()), so it doesn't mess up the time delta there.
                if let Some(last_time) = &mut gesture.dnd_last_event_time {
                    let now = self.clock.now_unadjusted();
                    if *last_time != now {
                        *last_time = now;

                        // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                        // updated the gesture already. Therefore, when this code runs, the pointer
                        // must be outside the DnD scrolling zone.
                        gesture.dnd_nonzero_start_time = None;
                    }
                }

                if let Some(anim) = &mut gesture.animation {
                    if anim.is_done() {
                        gesture.animation = None;
                    }
                }
            }
            None => (),
        }

        for ws in &mut self.workspaces {
            ws.advance_animations();
        }

        // After `clean_up_workspaces` above, so the strip's `should-show` is
        // evaluated against this frame's workspace count rather than lagging it.
        self.update_thumbnails_expand();
    }

    pub(super) fn are_animations_ongoing(&self) -> bool {
        self.workspace_switch
            .as_ref()
            .is_some_and(|s| s.is_animation_ongoing())
            || self.thumbnails_expand.is_some()
            || self.thumbnails_should_show() != self.thumbnails_shown
            || self.workspaces.iter().any(|ws| ws.are_animations_ongoing())
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.workspace_switch.is_some()
            // The expand only moves anything while the overview is in play — with
            // it closed the zoom is 1 regardless, so counting it would defer
            // pointer-focus refresh for a quarter second over a still screen.
            || (self.thumbnails_expand.is_some() && self.overview_progress.is_some())
            || self
                .workspaces
                .iter()
                .any(|ws| ws.are_transitions_ongoing())
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        if let Some(strip) = self.thumbnail_strip() {
            let rect = self.thumbnail_indicator_rect(&strip);
            let scale = self.scale.fractional_scale();
            let view_rect = Rectangle::new(rect.loc.upscale(-1.), self.view_size);
            self.thumb_indicator.update_render_elements(
                rect.size,
                true,
                true,
                false,
                view_rect,
                CornerRadius::from(12.),
                scale,
                1.,
            );

            if let Some(rect) = strip.placeholder {
                let view_rect = Rectangle::new(rect.loc.upscale(-1.), self.view_size);
                self.thumb_placeholder.update_render_elements(
                    rect.size,
                    true,
                    false,
                    false,
                    view_rect,
                    CornerRadius::from((thumbnails::PLACEHOLDER_WIDTH / 2.) as f32),
                    scale,
                    1.,
                );
            }
        }

        let mut insert_hint_ws_geo = None;
        let insert_hint_ws_id = self
            .insert_hint
            .as_ref()
            .and_then(|hint| hint.workspace.existing_id());

        for (ws, geo) in self.workspaces_with_render_geo_mut(true) {
            ws.update_render_elements(is_active);

            if Some(ws.id()) == insert_hint_ws_id {
                insert_hint_ws_geo = Some(geo);
            }
        }

        self.insert_hint_render_loc = None;
        if let Some(hint) = &self.insert_hint {
            match hint.workspace {
                InsertWorkspace::Existing(ws_id) => {
                    if let Some(ws) = self.workspaces.iter().find(|ws| ws.id() == ws_id) {
                        if let Some(mut area) = ws.insert_hint_area(hint.position) {
                            let scale = ws.scale().fractional_scale();
                            let view_size = ws.view_size();

                            // Make sure the hint is at least partially visible.
                            if matches!(hint.position, InsertPosition::NewColumn(_)) {
                                let zoom = self.overview_zoom();
                                let geo = insert_hint_ws_geo.unwrap();
                                let geo = geo.downscale(zoom);

                                area.loc.x = area.loc.x.max(-geo.loc.x - area.size.w / 2.);
                                area.loc.x =
                                    area.loc.x.min(geo.loc.x + geo.size.w - area.size.w / 2.);
                            }

                            // Round to physical pixels.
                            area = area.to_physical_precise_round(scale).to_logical(scale);

                            let view_rect = Rectangle::new(area.loc.upscale(-1.), view_size);
                            self.insert_hint_element.update_render_elements(
                                area.size,
                                view_rect,
                                hint.corner_radius,
                                scale,
                            );
                            self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                                workspace: hint.workspace,
                                location: area.loc,
                            });
                        }
                    } else {
                        error!("insert hint workspace missing from monitor");
                    }
                }
                InsertWorkspace::NewAt(ws_idx) => {
                    if hint.via_strip {
                        // The strip renders its drop placeholder instead of
                        // the between-workspaces bar.
                        return;
                    }

                    let scale = self.scale.fractional_scale();
                    let zoom = self.overview_zoom();
                    let gap = self.workspace_gap(zoom);

                    let hint_gap = round_logical_in_physical(scale, gap * 0.1);
                    let hint_thickness = gap - hint_gap * 2.;

                    let next_ws_geo = self.workspaces_render_geo().nth(ws_idx).unwrap();
                    // A bar across the gap: horizontal strips get a vertical
                    // bar, vertical strips a horizontal one.
                    let (hint_loc_diff, hint_size) = if self.workspaces_horizontal() {
                        let hint_length =
                            round_logical_in_physical(scale, next_ws_geo.size.h * 0.75);
                        let hint_y = round_logical_in_physical(
                            scale,
                            (next_ws_geo.size.h - hint_length) / 2.,
                        );
                        (
                            Point::from((hint_thickness + hint_gap, -hint_y)),
                            Size::from((hint_thickness, hint_length)),
                        )
                    } else {
                        let hint_length =
                            round_logical_in_physical(scale, next_ws_geo.size.w * 0.75);
                        let hint_x = round_logical_in_physical(
                            scale,
                            (next_ws_geo.size.w - hint_length) / 2.,
                        );
                        (
                            Point::from((-hint_x, hint_thickness + hint_gap)),
                            Size::from((hint_length, hint_thickness)),
                        )
                    };
                    let hint_loc = next_ws_geo.loc - hint_loc_diff;

                    // Sometimes the hint ends up 1 px wider than necessary and/or 1 px
                    // narrower than necessary. The values here seem correct. Might have to do with
                    // how zooming out currently doesn't round to output scale properly.

                    // Compute view rect as if we're above the next workspace (rather than below
                    // the previous one).
                    let view_rect = Rectangle::new(hint_loc_diff, next_ws_geo.size);

                    // In GNOME windowing mode the bar reads as a drop
                    // placeholder: give it the pill shape.
                    let radius = if self.options.layout.windowing_mode == WindowingMode::Floating {
                        CornerRadius::from((hint_size.w.min(hint_size.h) / 2.) as f32)
                    } else {
                        CornerRadius::default()
                    };
                    self.insert_hint_element
                        .update_render_elements(hint_size, view_rect, radius, scale);
                    self.insert_hint_render_loc = Some(InsertHintRenderLoc {
                        workspace: hint.workspace,
                        location: hint_loc,
                    });
                }
            }
        }
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let options =
            Rc::new(Options::clone(&base_options).with_merged_layout(self.layout_config.as_ref()));

        if self.options.layout.empty_workspace_above_first
            != options.layout.empty_workspace_above_first
            && self.workspaces.len() > 1
        {
            if options.layout.empty_workspace_above_first {
                self.add_workspace_top();
            } else if self.workspace_switch.is_none() && self.active_workspace_idx != 0 {
                self.workspaces.remove(0);
                self.active_workspace_idx = self.active_workspace_idx.saturating_sub(1);
            }
        }

        for ws in &mut self.workspaces {
            ws.update_config(options.clone());
        }

        self.insert_hint_element
            .update_config(options.layout.insert_hint);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<niri_config::LayoutPart>) -> bool {
        if self.layout_config == layout_config {
            return false;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());

        true
    }

    pub fn update_shaders(&mut self) {
        for ws in &mut self.workspaces {
            ws.update_shaders();
        }

        self.insert_hint_element.update_shaders();
    }

    pub fn update_output_size(&mut self) {
        self.scale = self.output.current_scale();
        self.view_size = output_size(&self.output);
        self.working_area = compute_working_area(&self.output, &self.options);

        for ws in &mut self.workspaces {
            ws.update_output_size();
        }
    }

    pub fn move_workspace_down(&mut self) {
        let mut new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if new_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && self.active_workspace_idx == 0 {
            self.add_workspace_top();
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_up(&mut self) {
        let mut new_idx = self.active_workspace_idx.saturating_sub(1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if self.active_workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        if self.options.layout.empty_workspace_above_first && new_idx == 0 {
            self.add_workspace_top();
            new_idx += 1;
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_to_idx(&mut self, old_idx: usize, new_idx: usize) {
        if self.workspaces.len() <= old_idx {
            return;
        }

        let mut new_idx = new_idx.clamp(0, self.workspaces.len() - 1);
        if old_idx == new_idx {
            return;
        }

        let ws = self.workspaces.remove(old_idx);
        self.workspaces.insert(new_idx, ws);

        if new_idx > old_idx {
            if new_idx == self.workspaces.len() - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom();
            }

            if self.options.layout.empty_workspace_above_first && old_idx == 0 {
                self.add_workspace_top();
                new_idx += 1;
            }
        } else {
            if old_idx == self.workspaces.len() - 1 {
                // Insert a new empty workspace.
                self.add_workspace_bottom();
            }

            if self.options.layout.empty_workspace_above_first && new_idx == 0 {
                self.add_workspace_top();
                new_idx += 1;
            }
        }

        // Only refocus the workspace if it was already focused
        if self.active_workspace_idx == old_idx {
            self.active_workspace_idx = new_idx;
        // If the workspace order was switched so that the current workspace moved down the
        // workspace stack, focus correctly
        } else if new_idx <= self.active_workspace_idx && old_idx > self.active_workspace_idx {
            self.active_workspace_idx += 1;
        } else if new_idx >= self.active_workspace_idx && old_idx < self.active_workspace_idx {
            self.active_workspace_idx = self.active_workspace_idx.saturating_sub(1);
        }

        self.workspace_switch = None;

        self.clean_up_workspaces();
    }

    /// Returns the geometry of the active window relative to and clamped to the output.
    ///
    /// During animations, assumes the final view position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.overview_open {
            return None;
        }

        self.active_workspace_ref().active_window_visual_rectangle()
    }

    fn workspace_size(&self, zoom: f64) -> Size<f64, Logical> {
        let ws_size = self.view_size.upscale(zoom);
        let scale = self.scale.fractional_scale();
        ws_size.to_physical_precise_ceil(scale).to_logical(scale)
    }

    /// GNOME (40+) arranges the overview workspaces in a horizontal row with
    /// the active one centered (gnome-shell `WorkspacesView`); niri's
    /// overview is a vertical strip. Applies to all workspace-strip geometry.
    pub(super) fn workspaces_horizontal(&self) -> bool {
        self.options.layout.windowing_mode == WindowingMode::Floating
    }

    fn workspace_gap(&self, zoom: f64) -> f64 {
        let scale = self.scale.fractional_scale();
        let gap = if self.workspaces_horizontal() {
            // gnome-shell `_getSpacing`: `availableSpace - workspaceWidth*0.4`
            // clamped to WORKSPACE_MIN_SPACING..WORKSPACE_MAX_SPACING (24..80).
            // On any normal aspect ratio that clamps to the minimum, which is
            // the point: the side margins stay free so the neighbor
            // workspaces peek in at the screen edges.
            let ws_width = self.view_size.w * zoom;
            let available = (self.view_size.w - ws_width) / 2.;
            (available - ws_width * 0.4).clamp(24., 80.)
        } else {
            self.view_size.h * 0.1 * zoom
        };
        round_logical_in_physical_max1(scale, gap)
    }

    fn workspace_size_with_gap(&self, zoom: f64) -> Size<f64, Logical> {
        let gap = self.workspace_gap(zoom);
        if self.workspaces_horizontal() {
            self.workspace_size(zoom) + Size::from((gap, 0.))
        } else {
            self.workspace_size(zoom) + Size::from((0., gap))
        }
    }

    /// The strip-axis extent of one workspace plus the gap.
    fn workspace_extent_with_gap(&self, zoom: f64) -> f64 {
        let size = self.workspace_size_with_gap(zoom);
        if self.workspaces_horizontal() {
            size.w
        } else {
            size.h
        }
    }

    /// The allocated box of every piece of overview chrome on this monitor
    /// (gnome-shell's `ControlsManagerLayout`). One place decides where the
    /// search entry, thumbnails, dash and window picker go; everything else
    /// consumes these boxes.
    pub fn controls_layout(&self) -> ControlsLayout {
        overview_layout::layout(
            self.view_size,
            self.working_area.loc.y,
            crate::ui::overview_search::PREFERRED_ENTRY_HEIGHT,
            crate::ui::dash::PREFERRED_HEIGHT,
            thumbnails::preferred_height(self.view_size, self.workspaces.len()),
            self.thumbnails_expand_fraction(),
        )
    }

    /// Whether the strip *wants* to be shown: gnome-shell's
    /// `ThumbnailsBox.shouldShow`, which with dynamic workspaces is purely a
    /// workspace count (it is not gated on the overview — that's the slide).
    fn thumbnails_should_show(&self) -> bool {
        self.workspaces.len() > thumbnails::NUM_WORKSPACES_THRESHOLD
    }

    fn thumbnails_expand_fraction(&self) -> f64 {
        match &self.thumbnails_expand {
            Some(anim) => anim.clamped_value().clamp(0., 1.),
            None => {
                if self.thumbnails_shown {
                    1.
                } else {
                    0.
                }
            }
        }
    }

    /// Starts (or retires) the expand ease when the strip's `should-show`
    /// flips — dragging a window onto the trailing empty workspace crosses the
    /// threshold *while the overview is open*, and the picker box depends on
    /// the band, so an instant flip would pop the workspace zoom.
    fn update_thumbnails_expand(&mut self) {
        let should_show = self.thumbnails_should_show();
        if should_show != self.thumbnails_shown {
            let from = self.thumbnails_expand_fraction();
            self.thumbnails_shown = should_show;
            // gnome-shell eases this one with `SIDE_CONTROLS_ANIMATION_TIME`
            // (250ms, EASE_OUT_QUAD, `overviewControls.js:360-366`) — a fixed
            // duration, not the configurable overview open/close animation. Only
            // whether animations run at all is inherited from the config.
            let config = niri_config::Animation {
                off: self.options.animations.overview_open_close.0.off,
                kind: niri_config::animations::Kind::Easing(
                    niri_config::animations::EasingParams {
                        duration_ms: 250,
                        curve: niri_config::animations::Curve::EaseOutQuad,
                    },
                ),
            };
            self.thumbnails_expand = Some(Animation::new(
                self.clock.clone(),
                from,
                if should_show { 1. } else { 0. },
                0.,
                config,
            ));
        }

        if self.thumbnails_expand.as_ref().is_some_and(|a| a.is_done()) {
            self.thumbnails_expand = None;
        }
    }

    /// The workspace zoom at an arbitrary overview progress.
    ///
    /// In GNOME mode the fully-zoomed-out size is not a constant: gnome-shell
    /// fits the workspace by height into whatever the window-picker box works
    /// out to, so the zoom follows the chrome. `progress` of `None` means the
    /// overview is closed (zoom 1).
    pub(super) fn zoom_at(&self, progress: Option<f64>) -> f64 {
        let zoom = if self.options.layout.windowing_mode == WindowingMode::Floating {
            // GNOME's overview geometry is by design, not configuration.
            self.controls_layout().workspaces.size.h / self.view_size.h
        } else {
            // Clamp to some sane values.
            self.options.overview.zoom.clamp(0.0001, 0.75)
        };

        compute_overview_zoom(zoom, progress)
    }

    pub fn overview_zoom(&self) -> f64 {
        let progress = self.overview_progress.as_ref().map(|p| p.value());
        self.zoom_at(progress)
    }

    /// Where the workspace row sits within the view, before the strip offset.
    ///
    /// Closed, the active workspace covers the view exactly (`zoom == 1`, so
    /// this is zero — a pointer against the screen edge at y = 0 must still hit
    /// it). Open, it lands on its allocated picker box. In between the two
    /// interpolate on the same progress that drives the zoom, which is how
    /// gnome-shell blends its `HIDDEN` and `WINDOW_PICKER` boxes
    /// (`overviewControls.js:207-216`).
    fn workspaces_static_offset(&self, zoom: f64) -> Point<f64, Logical> {
        let ws_size = self.workspace_size(zoom);
        // Full width is available, so the row stays horizontally centered.
        let x = (self.view_size.w - ws_size.w) / 2.;

        let y = if self.options.layout.windowing_mode == WindowingMode::Floating {
            let progress = self.overview_progress.as_ref().map_or(0., |p| p.value());
            self.controls_layout().workspaces.loc.y * progress
        } else {
            (self.view_size.h - ws_size.h) / 2.
        };

        let scale = self.scale.fractional_scale();
        Point::from((x, y))
            .to_physical_precise_round(scale)
            .to_logical(scale)
    }

    /// In GNOME windowing mode, the overview spreads each workspace's windows
    /// into picker slots (gnome-shell's window picker); this is how far along
    /// that spread is, if it's on at all.
    pub fn expose_progress(&self) -> Option<f64> {
        if self.options.layout.windowing_mode != WindowingMode::Floating {
            return None;
        }
        let progress = self.overview_progress.as_ref()?;
        let progress = progress.clamped_value().clamp(0., 1.);
        (progress > 0.).then_some(progress)
    }

    /// Whether the overview shows the workspace thumbnails strip: GNOME's
    /// dynamic workspaces show it only once a second desktop is populated
    /// (more workspaces than the threshold, counting the trailing empty).
    pub fn thumbnails_visible(&self) -> bool {
        self.expose_progress().is_some() && self.thumbnails_expand_fraction() > 0.
    }

    /// The thumbnails strip, laid out in the band [`Self::controls_layout`]
    /// allocates it below the search entry. While a drag hovers one of its
    /// gaps, the strip makes room for the new-workspace drop placeholder there.
    pub fn thumbnail_strip(&self) -> Option<Strip> {
        if !self.thumbnails_visible() {
            return None;
        }
        let placeholder = self.insert_hint.as_ref().and_then(|hint| {
            if !hint.via_strip {
                return None;
            }
            match hint.workspace {
                InsertWorkspace::NewAt(idx) => Some(idx),
                InsertWorkspace::Existing(_) => None,
            }
        });
        Some(thumbnails::strip_geometry(
            self.view_size,
            self.controls_layout().thumbnails,
            self.workspaces.len(),
            placeholder,
        ))
    }

    /// The strip slides in from above the screen with the overview
    /// transition (gnome-shell translates its box in and out).
    ///
    /// Divergence: gnome-shell instead eases `expandFraction` into the box
    /// height when the strip appears mid-overview; we fold the expand into the
    /// same slide, so the strip slides down as the picker makes room for it.
    fn thumbnail_slide_offset(&self, strip: &Strip, progress: f64) -> f64 {
        let progress = progress * self.thumbnails_expand_fraction();
        let bounds = strip.bounds();
        let extent = bounds.loc.y + bounds.size.h + thumbnails::INDICATOR_WIDTH;
        -extent * (1. - progress)
    }

    /// The thumbnail rect the active-workspace indicator wraps,
    /// interpolated between the two nearest thumbnails while the view moves
    /// (gnome-shell tracks the scroll adjustment).
    fn thumbnail_indicator_rect(&self, strip: &Strip) -> Rectangle<f64, Logical> {
        let last = (strip.thumbs.len() - 1) as f64;
        let idx = self.workspace_render_idx().clamp(0., last);
        let (lo, hi) = (idx.floor() as usize, idx.ceil() as usize);
        let t = idx.fract();
        let a = strip.thumbs[lo];
        let b = strip.thumbs[hi];
        Rectangle::new(
            Point::from((
                a.loc.x + (b.loc.x - a.loc.x) * t,
                a.loc.y + (b.loc.y - a.loc.y) * t,
            )),
            a.size,
        )
    }

    /// The workspace whose strip thumbnail is under the position.
    pub fn thumbnail_workspace_under(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        let strip = self.thumbnail_strip()?;
        let idx = strip.thumb_under(pos_within_output)?;
        Some(&self.workspaces[idx])
    }

    /// Recolors the accent-colored overview chrome (`org.gnome.desktop.interface
    /// accent-color`).
    pub fn set_gnome_accent_color(&mut self, accent: [u8; 3]) {
        self.thumb_indicator
            .update_config(thumbnail_indicator_config(accent));
    }

    pub(super) fn set_overview_progress(&mut self, progress: Option<&super::OverviewProgress>) {
        let prev_render_idx = self.workspace_render_idx();
        self.overview_progress = progress.map(OverviewProgress::from);
        let new_render_idx = self.workspace_render_idx();

        // If the view jumped (can happen when going from corrected to uncorrected render_idx, for
        // example when toggling the overview in the middle of an overview animation), then restart
        // the workspace switch to avoid jumps.
        if prev_render_idx != new_render_idx {
            if let Some(WorkspaceSwitch::Animation(anim)) = &mut self.workspace_switch {
                // FIXME: maintain velocity.
                *anim = anim.restarted(prev_render_idx, anim.to(), 0.);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn overview_progress_value(&self) -> Option<f64> {
        self.overview_progress.as_ref().map(|p| p.value())
    }

    pub fn workspace_render_idx(&self) -> f64 {
        // If workspace switch and overview progress are matching animations, then compute a
        // correction term to make the movement appear monotonic.
        if let (
            Some(WorkspaceSwitch::Animation(switch_anim)),
            Some(OverviewProgress::Animation(progress_anim)),
        ) = (&self.workspace_switch, &self.overview_progress)
        {
            if switch_anim.start_time() == progress_anim.start_time()
                && (switch_anim.duration().as_secs_f64() - progress_anim.duration().as_secs_f64())
                    .abs()
                    <= 0.001
            {
                #[rustfmt::skip]
                // How this was derived:
                //
                // - Assume we're animating a zoom + switch. Consider switch "from" and "to".
                //   These are render_idx values, so first workspace to second would have switch
                //   from = 0. and to = 1. regardless of the zoom level.
                //
                // - At the start, the point at "from" is at Y = 0. We're moving the point at "to"
                //   to Y = 0. We want this to be a monotonic motion in apparent coordinates (after
                //   zoom).
                //
                // - Height at the start:
                //   from_height = (size.h + gap) * from_zoom.
                //
                // - Current height:
                //   current_height = (size.h + gap) * zoom.
                //
                // - We're moving the "to" point to Y = 0:
                //   to_y = 0.
                //
                // - The initial position of the point we're moving:
                //   from_y = (to - from) * from_height.
                //
                // - We want this point to travel monotonically in apparent coordinates:
                //   current_y = from_y + (to_y - from_y) * progress,
                //   where progress is from 0 to 1, equals to the animation progress (switch and
                //   zoom are the same since they are synchronized).
                //
                // - Derive the Y of the first workspace from this:
                //   first_y = current_y - to * current_height.
                //
                // Now, let's substitute and rearrange the terms.
                //
                // - current_y = from_y + (0 - (to - from) * from_height) * progress
                // - progress = (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - (to - from) * from_height * (switch_anim.value() - from) / (to - from)
                // - current_y = from_y - from_height * (switch_anim.value() - from)
                // - first_y = from_y - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = (to - from) * from_height - from_height * (switch_anim.value() - from) - to * current_height
                // - first_y = to * from_height - switch_anim.value() * from_height - to * current_height
                // - first_y = -switch_anim.value() * from_height + to * (from_height - current_height)
                let from = progress_anim.from();
                let from_zoom = self.zoom_at(Some(from));
                let from_ws_extent_with_gap = self.workspace_extent_with_gap(from_zoom);

                let zoom = self.overview_zoom();
                let ws_extent_with_gap = self.workspace_extent_with_gap(zoom);

                let first_ws_pos = -switch_anim.value() * from_ws_extent_with_gap
                    + switch_anim.to() * (from_ws_extent_with_gap - ws_extent_with_gap);

                return -first_ws_pos / ws_extent_with_gap;
            }
        };

        if let Some(switch) = &self.workspace_switch {
            switch.current_idx()
        } else {
            self.active_workspace_idx as f64
        }
    }

    pub fn workspaces_render_geo(&self) -> impl Iterator<Item = Rectangle<f64, Logical>> {
        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();
        let horizontal = self.workspaces_horizontal();

        let ws_size = self.workspace_size(zoom);
        let ws_extent_with_gap = self.workspace_extent_with_gap(zoom);

        let static_offset = self.workspaces_static_offset(zoom);

        let first_ws_pos = -self.workspace_render_idx() * ws_extent_with_gap;
        let first_ws_pos = round_logical_in_physical(scale, first_ws_pos);

        // Return position for one-past-last workspace too.
        (0..=self.workspaces.len()).map(move |idx| {
            let pos = first_ws_pos + idx as f64 * ws_extent_with_gap;
            let loc = if horizontal {
                Point::from((pos, 0.))
            } else {
                Point::from((0., pos))
            };
            let loc = loc + static_offset;

            // Even though all components that go into loc are rounded to physical pixels, the
            // floating point addition may lose precision. This can result for example in the
            // current workspace having y = 0.0000000000002 and thus missing pointer hits at the
            // monitor edge with y = 0. So, post-round the location too.
            let loc = loc.to_physical_precise_round(scale).to_logical(scale);

            Rectangle::new(loc, ws_size)
        })
    }

    pub fn workspaces_with_render_geo(
        &self,
    ) -> impl Iterator<Item = (&Workspace<W>, Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo_idx(
        &self,
    ) -> impl Iterator<Item = ((usize, &Workspace<W>), Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter().enumerate(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| geo.intersection(output_geo).is_some())
    }

    pub fn workspaces_with_render_geo_mut(
        &mut self,
        cull: bool,
    ) -> impl Iterator<Item = (&mut Workspace<W>, Rectangle<f64, Logical>)> {
        let output_geo = Rectangle::from_size(self.view_size);

        let geo = self.workspaces_render_geo();
        zip(self.workspaces.iter_mut(), geo)
            // Cull out workspaces outside the output.
            .filter(move |(_ws, geo)| !cull || geo.intersection(output_geo).is_some())
    }

    pub fn workspace_under(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(&Workspace<W>, Rectangle<f64, Logical>)> {
        let horizontal = self.workspaces_horizontal();
        let (ws, geo) = self.workspaces_with_render_geo().find_map(|(ws, geo)| {
            // Extend the cross axis to the entire output.
            let bounds = if horizontal {
                Rectangle::new(
                    Point::from((geo.loc.x, 0.)),
                    Size::from((geo.size.w, self.view_size.h)),
                )
            } else {
                Rectangle::new(
                    Point::from((0., geo.loc.y)),
                    Size::from((self.view_size.w, geo.size.h)),
                )
            };

            bounds.contains(pos_within_output).then_some((ws, geo))
        })?;
        Some((ws, geo))
    }

    pub fn workspace_under_narrow(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<&Workspace<W>> {
        self.workspaces_with_render_geo()
            .find_map(|(ws, geo)| geo.contains(pos_within_output).then_some(ws))
    }

    pub fn window_under(&self, pos_within_output: Point<f64, Logical>) -> Option<(&W, HitType)> {
        let (ws, geo) = self.workspace_under(pos_within_output)?;

        if self.overview_progress.is_some() {
            let zoom = self.overview_zoom();
            let pos_within_workspace = (pos_within_output - geo.loc).downscale(zoom);
            // In GNOME windowing mode the overview spreads windows into
            // picker slots; hit-test those.
            let (win, hit) = if self.expose_progress().is_some() {
                ws.window_under_expose(pos_within_workspace)?
            } else {
                ws.window_under(pos_within_workspace)?
            };
            // During the overview animation, we cannot do input hits because we cannot really
            // represent scaled windows properly.
            Some((win, hit.to_activate()))
        } else {
            let (win, hit) = ws.window_under(pos_within_output - geo.loc)?;
            Some((win, hit.offset_win_pos(geo.loc)))
        }
    }

    pub fn resize_edges_under(&self, pos_within_output: Point<f64, Logical>) -> Option<ResizeEdge> {
        if self.overview_progress.is_some() {
            return None;
        }

        let (ws, geo) = self.workspace_under(pos_within_output)?;
        ws.resize_edges_under(pos_within_output - geo.loc)
    }

    pub(super) fn insert_position(
        &self,
        pos_within_output: Point<f64, Logical>,
    ) -> (InsertWorkspace, Rectangle<f64, Logical>) {
        // The thumbnails strip takes drops too: onto a thumbnail, or into a
        // gap to insert a workspace there (gnome-shell's ThumbnailsBox
        // acceptDrop).
        if let Some(strip) = self.thumbnail_strip() {
            match strip.drop_target(pos_within_output) {
                Some(thumbnails::DropTarget::Workspace(idx)) => {
                    return (
                        InsertWorkspace::Existing(self.workspaces[idx].id()),
                        strip.thumbs[idx],
                    );
                }
                Some(thumbnails::DropTarget::NewAt(idx)) => {
                    return (InsertWorkspace::NewAt(idx), Rectangle::default());
                }
                None => (),
            }
        }

        let horizontal = self.workspaces_horizontal();

        // Strip-axis coordinates: where the pointer is, and a rect's span.
        let pos = if horizontal {
            pos_within_output.x
        } else {
            pos_within_output.y
        };
        let span = move |geo: Rectangle<f64, Logical>| {
            if horizontal {
                (geo.loc.x, geo.loc.x + geo.size.w)
            } else {
                (geo.loc.y, geo.loc.y + geo.size.h)
            }
        };
        let contains = move |geo: Rectangle<f64, Logical>| {
            let (start, end) = span(geo);
            start <= pos && pos < end
        };

        let mut iter = self.workspaces_with_render_geo_idx();

        let dummy = Rectangle::default();

        // Monitors always have at least one workspace.
        let ((idx, ws), geo) = iter.next().unwrap();

        // Check if before first.
        if pos < span(geo).0 {
            return (InsertWorkspace::NewAt(idx), dummy);
        }

        // Check first.
        if contains(geo) {
            return (InsertWorkspace::Existing(ws.id()), geo);
        }

        let mut last_geo = geo;
        let mut last_idx = idx;
        for ((idx, ws), geo) in iter {
            // Check the gap before this workspace.
            let gap_geo = if horizontal {
                let gap_loc = Point::from((last_geo.loc.x + last_geo.size.w, last_geo.loc.y));
                let gap_size = Size::from((geo.loc.x - gap_loc.x, geo.size.h));
                Rectangle::new(gap_loc, gap_size)
            } else {
                let gap_loc = Point::from((last_geo.loc.x, last_geo.loc.y + last_geo.size.h));
                let gap_size = Size::from((geo.size.w, geo.loc.y - gap_loc.y));
                Rectangle::new(gap_loc, gap_size)
            };
            if contains(gap_geo) {
                return (InsertWorkspace::NewAt(idx), dummy);
            }

            // Check workspace itself.
            if contains(geo) {
                return (InsertWorkspace::Existing(ws.id()), geo);
            }

            last_geo = geo;
            last_idx = idx;
        }

        // Anything past the last one.
        (InsertWorkspace::NewAt(last_idx + 1), dummy)
    }

    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer only if the view is stationary.
        if self.workspace_switch.is_some() || self.overview_progress.is_some() {
            return false;
        }

        let ws = &self.workspaces[self.active_workspace_idx];
        ws.render_above_top_layer()
    }

    pub fn render_insert_hint_between_workspaces(
        &self,
        push: &mut dyn FnMut(MonitorRenderElement),
    ) {
        if self.options.layout.insert_hint.off {
            return;
        }
        let Some(render_loc) = self.insert_hint_render_loc else {
            return;
        };
        let InsertWorkspace::NewAt(_) = render_loc.workspace else {
            return;
        };

        self.insert_hint_element
            .render(render_loc.location, &mut |elem| {
                let elem = MonitorInnerRenderElement::Ring(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                let elem =
                    RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
                push(elem);
            });
    }

    /// Renders the overview workspace thumbnails strip: each workspace in
    /// miniature (windows at their real positions over the wallpaper), the
    /// active one wrapped by the indicator ring.
    pub fn render_thumbnails(
        &self,
        mut ctx: RenderCtx,
        wallpaper: Option<&Wallpaper>,
        push: &mut dyn FnMut(MonitorRenderElement),
    ) {
        let Some(strip) = self.thumbnail_strip() else {
            return;
        };
        let Some(progress) = self.expose_progress() else {
            return;
        };
        let _span = tracy_client::span!("Monitor::render_thumbnails");

        let scale = self.scale.fractional_scale();
        let slide = Point::from((0., self.thumbnail_slide_offset(&strip, progress)));

        // First pushed = topmost: the indicator ring sits above the
        // thumbnails (gnome-shell keeps it as the top sibling).
        let indicator_loc = self.thumbnail_indicator_rect(&strip).loc + slide;
        self.thumb_indicator.render(indicator_loc, &mut |elem| {
            let elem = MonitorInnerRenderElement::Ring(elem);
            let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
            let elem =
                RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
            push(elem);
        });

        // The new-workspace drop placeholder, while a drag hovers a gap.
        if let Some(rect) = strip.placeholder {
            self.thumb_placeholder
                .render(rect.loc + slide, &mut |elem| {
                    let elem = MonitorInnerRenderElement::Ring(elem);
                    let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                    let elem = RelocateRenderElement::from_element(
                        elem,
                        Point::default(),
                        Relocate::Relative,
                    );
                    push(elem);
                });
        }

        // Clip each miniature to its workspace.
        let crop_bounds = Rectangle::from_size(self.view_size).to_physical_precise_round(scale);

        for (ws, thumb) in zip(&self.workspaces, &strip.thumbs) {
            let thumb = Rectangle::new(thumb.loc + slide, thumb.size);
            let thumb_loc_physical = thumb.loc.to_physical_precise_round(scale);
            let xray_pos = XrayPos::new(thumb.loc, strip.scale);

            macro_rules! push_thumb {
                () => {{
                    &mut |elem| {
                        if let Some(elem) =
                            CropRenderElement::from_element(elem, scale, crop_bounds)
                        {
                            let elem = MonitorInnerRenderElement::from(elem);
                            let elem = RescaleRenderElement::from_element(
                                elem,
                                Point::from((0, 0)),
                                strip.scale,
                            );
                            let elem = RelocateRenderElement::from_element(
                                elem,
                                thumb_loc_physical,
                                Relocate::Relative,
                            );
                            push(elem);
                        }
                    }
                }};
            }

            // Same layer order as the workspace itself.
            if ws.scrolling_renders_on_top() {
                ws.render_scrolling(ctx.r(), xray_pos, false, push_thumb!());
                ws.render_floating(ctx.r(), xray_pos, false, push_thumb!());
            } else {
                ws.render_floating(ctx.r(), xray_pos, false, push_thumb!());
                ws.render_scrolling(ctx.r(), xray_pos, false, push_thumb!());
            }

            // The wallpaper behind, rounded like the thumbnail (half the
            // indicator's radius, per the shell theme); the solid color
            // backs workspaces without one.
            let mut wallpapered = false;
            if let Some(wallpaper) = wallpaper {
                let radius = 6. / strip.scale;
                if let Some(elem) = wallpaper.render(
                    ctx.renderer,
                    ws.view_size(),
                    radius,
                    Scale::from(scale * strip.scale),
                ) {
                    let elem = MonitorInnerRenderElement::RoundedTexture(elem);
                    let elem =
                        RescaleRenderElement::from_element(elem, Point::from((0, 0)), strip.scale);
                    let elem = RelocateRenderElement::from_element(
                        elem,
                        thumb_loc_physical,
                        Relocate::Relative,
                    );
                    push(elem);
                    wallpapered = true;
                }
            }
            if !wallpapered {
                let elem = MonitorInnerRenderElement::SolidColor(ws.render_background());
                let elem =
                    RescaleRenderElement::from_element(elem, Point::from((0, 0)), strip.scale);
                let elem = RelocateRenderElement::from_element(
                    elem,
                    thumb_loc_physical,
                    Relocate::Relative,
                );
                push(elem);
            }
        }
    }

    pub fn render_workspaces(
        &self,
        mut ctx: RenderCtx,
        focus_ring: bool,
        push: &mut dyn FnMut(MonitorRenderElement),
    ) {
        let _span = tracy_client::span!("Monitor::render_workspaces");

        let scale = self.scale.fractional_scale();
        // Ceil the height in physical pixels.
        let height = (self.view_size.h * scale).ceil() as i32;

        // Crop the elements to prevent them overflowing, currently visible during a workspace
        // switch.
        //
        // HACK: crop to infinite bounds at least horizontally where we
        // know there's no workspace joining or monitor bounds, otherwise
        // it will cut pixel shaders and mess up the coordinate space.
        // There's also a damage tracking bug which causes glitched
        // rendering for maximized GTK windows.
        //
        // FIXME: use proper bounds after fixing the Crop element.
        //
        // The exact crop goes on the axis the workspaces join on, so a window
        // poking out of one workspace doesn't draw over its neighbor.
        let crop_bounds = if self.workspace_switch.is_some() || self.overview_progress.is_some() {
            if self.workspaces_horizontal() {
                let width = (self.view_size.w * scale).ceil() as i32;
                Rectangle::new(
                    Point::from((0, -i32::MAX / 2)),
                    Size::from((width, i32::MAX)),
                )
            } else {
                Rectangle::new(
                    Point::from((-i32::MAX / 2, 0)),
                    Size::from((i32::MAX, height)),
                )
            }
        } else {
            Rectangle::new(
                Point::from((-i32::MAX / 2, -i32::MAX / 2)),
                Size::from((i32::MAX, i32::MAX)),
            )
        };

        let zoom = self.overview_zoom();

        let insert_hint_render_loc = self
            .insert_hint_render_loc
            .filter(|_| !self.options.layout.insert_hint.off);

        let scale_relocate = move |geo: Rectangle<f64, Logical>, elem| {
            let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
            RelocateRenderElement::from_element(
                elem,
                // The offset we get from workspaces_with_render_geo() is already
                // rounded to physical pixels, but it's in the logical coordinate
                // space, so we need to convert it to physical.
                geo.loc.to_physical_precise_round(scale),
                Relocate::Relative,
            )
        };

        for (ws, geo) in self.workspaces_with_render_geo() {
            // Macro instead of closure because ws and insert hint have different elem types.
            macro_rules! push {
                () => {{
                    &mut |elem| {
                        let elem = CropRenderElement::from_element(elem, scale, crop_bounds);
                        if let Some(elem) = elem {
                            let elem = MonitorInnerRenderElement::from(elem);
                            push(scale_relocate(geo, elem));
                        }
                    }
                }};
            }

            let xray_pos = XrayPos::new(geo.loc, zoom);

            macro_rules! push_hint {
                () => {
                    if let Some(loc) = insert_hint_render_loc {
                        if loc.workspace == InsertWorkspace::Existing(ws.id()) {
                            self.insert_hint_element.render(loc.location, push!());
                        }
                    }
                };
            }

            // In GNOME windowing mode the overview renders the window picker
            // instead of the layers at their layout positions.
            if let Some(progress) = self.expose_progress() {
                push_hint!();
                ws.render_expose(ctx.r(), xray_pos, progress, push!());
                continue;
            }

            // First pushed = topmost. The scrolling insert hint goes between
            // the floating and scrolling layers; the edge-tile preview goes
            // above both, just below the dragged window (mutter's TilePreview
            // sits directly below the window actor). Same when the scrolling
            // layer renders on top: the hint must not hide under it.
            let hint_above_all = ws.scrolling_renders_on_top()
                || self
                    .insert_hint
                    .as_ref()
                    .is_some_and(|hint| matches!(hint.position, InsertPosition::EdgeTile(_)));
            if hint_above_all {
                push_hint!();
            }
            if ws.scrolling_renders_on_top() {
                ws.render_scrolling(ctx.r(), xray_pos, focus_ring, push!());
                ws.render_floating(ctx.r(), xray_pos, focus_ring, push!());
            } else {
                ws.render_floating(ctx.r(), xray_pos, focus_ring, push!());
                if !hint_above_all {
                    push_hint!();
                }
                ws.render_scrolling(ctx.r(), xray_pos, focus_ring, push!());
            }
        }
    }

    pub fn render_workspace_shadows(&self, push: &mut dyn FnMut(MonitorRenderElement)) {
        let Some(progress) = self.overview_progress.as_ref().map(|p| p.clamped_value()) else {
            return;
        };
        let alpha = progress.clamp(0., 1.) as f32;

        let _span = tracy_client::span!("Monitor::render_workspace_shadows");

        let scale = self.scale.fractional_scale();
        let zoom = self.overview_zoom();

        for (ws, geo) in self.workspaces_with_render_geo() {
            ws.render_shadow(&mut |elem| {
                let elem = elem.with_alpha(alpha);
                let elem = MonitorInnerRenderElement::Shadow(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
                let elem = RelocateRenderElement::from_element(
                    elem,
                    geo.loc.to_physical_precise_round(scale),
                    Relocate::Relative,
                );
                push(elem);
            });
        }
    }

    pub fn workspace_switch_gesture_begin(&mut self, is_touchpad: bool) {
        let center_idx = self.active_workspace_idx;
        let current_idx = self.workspace_render_idx();

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad,
            is_clamped: !self.overview_open,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
            dnd_snap_last_switch: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if let Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
            dnd_last_event_time: Some(_),
            ..
        })) = &self.workspace_switch
        {
            // Already active.
            return;
        }

        if !self.overview_open {
            // This gesture is only for the overview.
            return;
        }

        let center_idx = self.active_workspace_idx;
        let current_idx = self.workspace_render_idx();

        let gesture = WorkspaceSwitchGesture {
            center_idx,
            start_idx: current_idx,
            current_idx,
            animation: None,
            tracker: SwipeTracker::new(),
            is_touchpad: false,
            is_clamped: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
            dnd_snap_last_switch: None,
        };
        self.workspace_switch = Some(WorkspaceSwitch::Gesture(gesture));
    }

    pub fn workspace_switch_gesture_update(
        &mut self,
        delta_y: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return None;
        };

        // Reduce the effect of zoom on the touchpad somewhat.
        let delta_scale = if gesture.is_touchpad {
            (zoom - 1.) / 2.5 + 1.
        } else {
            zoom
        };

        let delta_y = delta_y / delta_scale;
        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        gesture.tracker.push(delta_y, timestamp);

        let pos = gesture.tracker.pos() / total_height;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let new_idx = gesture.start_idx + pos;
        let new_idx = rubber_band.clamp(min, max, new_idx);

        if gesture.current_idx == new_idx {
            return Some(false);
        }

        gesture.current_idx = new_idx;
        Some(true)
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
        let zoom = self.overview_zoom();
        // The strip is not necessarily centered in the view (in GNOME mode it
        // sits in its allocated picker box), so take the cross-axis band from
        // the same offset the row is rendered at.
        let offset = self.workspaces_static_offset(zoom);

        let gnome_mode = self.options.layout.windowing_mode == WindowingMode::Floating;

        // In GNOME windowing mode the desktop's screen edges belong to edge
        // tiling; the DnD edge scroll only runs in the overview.
        if gnome_mode && self.overview_progress.is_none() {
            return false;
        }

        let horizontal = self.workspaces_horizontal();

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        let Some(last_time) = gesture.dnd_last_event_time else {
            // Not a DnD scroll.
            return false;
        };

        let config = &self.options.gestures.dnd_edge_workspace_switch;
        let trigger_height = config.trigger_height;

        // The trigger zones sit at the ends of the axis the workspaces are
        // laid out on. Restrict the cross axis to the strip of workspaces to
        // avoid unwanted trigger after using the hot corner or during
        // cross-axis scroll; consider the working area on the main axis so
        // layer-shell docks and such don't prevent scrolling.
        let (main, extent, cross, cross_extent) = if horizontal {
            let cross_extent = self.view_size.h * zoom;
            let cross = pos.y - offset.y;
            let main = pos.x - self.working_area.loc.x;
            (main, self.working_area.size.w, cross, cross_extent)
        } else {
            let cross_extent = self.view_size.w * zoom;
            let cross = pos.x - offset.x;
            let main = pos.y - self.working_area.loc.y;
            (main, self.working_area.size.h, cross, cross_extent)
        };

        let main = main.clamp(0., extent);
        let trigger_height = trigger_height.clamp(0., extent / 2.);

        let delta = if cross < 0. || cross_extent <= cross {
            // Outside the bounds on the cross axis.
            0.
        } else if main < trigger_height {
            -(trigger_height - main)
        } else if extent - main < trigger_height {
            trigger_height - (extent - main)
        } else {
            0.
        };

        let delta = if trigger_height < 0.01 {
            // Sanity check for trigger-height 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_height
        };
        let delta = delta * speed;

        let now = self.clock.now_unadjusted();
        gesture.dnd_last_event_time = Some(now);

        if delta == 0. {
            // We're outside the scrolling zone.
            gesture.dnd_nonzero_start_time = None;
            return false;
        }

        let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);

        // Delay starting the gesture a bit to avoid unwanted movement when dragging across
        // monitors.
        let delay = Duration::from_millis(u64::from(config.delay_ms));
        if now.saturating_sub(nonzero_start) < delay {
            return true;
        }

        // In GNOME mode, snap one workspace at a time instead of panning:
        // switch right away on entering the trigger zone, then wait out a
        // grace period before snapping again while the pointer stays there.
        if gnome_mode {
            let due = match gesture.dnd_snap_last_switch {
                None => true,
                Some(last) => now.saturating_sub(last) >= WORKSPACE_DND_EDGE_SNAP_GRACE,
            };
            if !due {
                return true;
            }

            let target = if delta < 0. {
                self.active_workspace_idx.checked_sub(1)
            } else {
                Some(self.active_workspace_idx + 1).filter(|idx| *idx < self.workspaces.len())
            };
            let Some(target) = target else {
                // Nothing beyond this end.
                return true;
            };

            gesture.dnd_snap_last_switch = Some(now);
            // activate_workspace() animates within the ongoing DnD gesture,
            // so the gesture (and this snap state) survives the switch.
            self.activate_workspace(target);
            return true;
        }

        let time_delta = now.saturating_sub(last_time).as_secs_f64();

        let delta = delta * time_delta * config.max_speed;

        gesture.tracker.push(delta, now);

        let total_height = WORKSPACE_DND_EDGE_SCROLL_MOVEMENT;
        let pos = gesture.tracker.pos() / total_height;
        let unclamped = gesture.start_idx + pos;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let clamped = unclamped.clamp(min, max);

        // Make sure that DnD scrolling too much outside the min/max does not "build up".
        gesture.start_idx += clamped - unclamped;
        gesture.current_idx = clamped;

        true
    }

    pub fn workspace_switch_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        let Some(WorkspaceSwitch::Gesture(gesture)) = &self.workspace_switch else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        let zoom = self.overview_zoom();
        let total_height = if gesture.dnd_last_event_time.is_some() {
            WORKSPACE_DND_EDGE_SCROLL_MOVEMENT
        } else if gesture.is_touchpad {
            WORKSPACE_GESTURE_MOVEMENT
        } else {
            self.workspace_size_with_gap(1.).h
        };

        let Some(WorkspaceSwitch::Gesture(gesture)) = &mut self.workspace_switch else {
            return false;
        };

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let mut rubber_band = WORKSPACE_GESTURE_RUBBER_BAND;
        rubber_band.limit /= zoom;

        let mut velocity = gesture.tracker.velocity() / total_height;
        let current_pos = gesture.tracker.pos() / total_height;
        let pos = gesture.tracker.projected_end_pos() / total_height;

        let (min, max) = gesture.min_max(self.workspaces.len());
        let new_idx = gesture.start_idx + pos;

        let new_idx = new_idx.clamp(min, max);
        let new_idx = new_idx.round() as usize;

        velocity *= rubber_band.clamp_derivative(min, max, gesture.start_idx + current_pos);

        if self.active_workspace_idx != new_idx {
            self.previous_workspace_id = Some(self.workspaces[self.active_workspace_idx].id());
        }

        self.active_workspace_idx = new_idx;
        self.workspace_switch = Some(WorkspaceSwitch::Animation(Animation::new(
            self.clock.clone(),
            gesture.current_idx,
            new_idx as f64,
            velocity,
            self.options.animations.workspace_switch.0,
        )));

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        if !matches!(
            self.workspace_switch,
            Some(WorkspaceSwitch::Gesture(WorkspaceSwitchGesture {
                dnd_last_event_time: Some(_),
                ..
            }))
        ) {
            // Not a DnD scroll.
            return;
        };

        self.workspace_switch_gesture_end(None);
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn layout_config(&self) -> Option<&niri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub(super) fn verify_invariants(&self) {
        use approx::assert_abs_diff_eq;

        let options =
            Options::clone(&self.base_options).with_merged_layout(self.layout_config.as_ref());
        assert_eq!(&*self.options, &options);

        assert!(
            !self.workspaces.is_empty(),
            "monitor must have at least one workspace"
        );
        assert!(self.active_workspace_idx < self.workspaces.len());

        if let Some(WorkspaceSwitch::Animation(anim)) = &self.workspace_switch {
            let before_idx = anim.from() as usize;
            let after_idx = anim.to() as usize;

            assert!(before_idx < self.workspaces.len());
            assert!(after_idx < self.workspaces.len());
        }

        assert!(
            !self.workspaces.last().unwrap().has_windows(),
            "monitor must have an empty workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                !self.workspaces.first().unwrap().has_windows(),
                "first workspace must be empty when empty_workspace_above_first is set"
            )
        }

        assert!(
            self.workspaces.last().unwrap().name.is_none(),
            "monitor must have an unnamed workspace in the end"
        );
        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspaces.first().unwrap().name.is_none(),
                "first workspace must be unnamed when empty_workspace_above_first is set"
            )
        }

        if self.options.layout.empty_workspace_above_first {
            assert!(
                self.workspaces.len() != 2,
                "if empty_workspace_above_first is set there must be just 1 or 3+ workspaces"
            )
        }

        // If there's no workspace switch in progress, there can't be any non-last non-active
        // empty workspaces. If empty_workspace_above_first is set then the first workspace
        // will be empty too.
        let pre_skip = if self.options.layout.empty_workspace_above_first {
            1
        } else {
            0
        };
        if self.workspace_switch.is_none() {
            for (idx, ws) in self
                .workspaces
                .iter()
                .enumerate()
                .skip(pre_skip)
                .rev()
                // skip last
                .skip(1)
            {
                if idx != self.active_workspace_idx {
                    assert!(
                        ws.has_windows_or_name(),
                        "non-active workspace can't be empty and unnamed except the last one"
                    );
                }
            }
        }

        for workspace in &self.workspaces {
            assert_eq!(self.clock, workspace.clock);

            assert_eq!(
                self.scale().integer_scale(),
                workspace.scale().integer_scale()
            );
            assert_eq!(
                self.scale().fractional_scale(),
                workspace.scale().fractional_scale()
            );
            assert_eq!(self.view_size, workspace.view_size());
            assert_eq!(self.working_area, workspace.working_area());

            assert_eq!(
                workspace.base_options, self.options,
                "workspace options must be synchronized with monitor"
            );
        }

        let scale = self.scale().fractional_scale();
        let iter = self.workspaces_with_render_geo();
        for (_ws, ws_geo) in iter {
            let pos = ws_geo.loc;
            let rounded_pos = pos.to_physical_precise_round(scale).to_logical(scale);

            // Workspace positions must be rounded to physical pixels.
            assert_abs_diff_eq!(pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(pos.y, rounded_pos.y, epsilon = 1e-5);
        }
    }
}

/// The strip's drop placeholder: a translucent pill marking where the new
/// workspace goes (gnome-shell's workspace-placeholder asset).
fn thumbnail_placeholder_config() -> niri_config::FocusRing {
    let color = niri_config::Color::from_rgba8_unpremul(0xff, 0xff, 0xff, 0x66);
    niri_config::FocusRing {
        off: false,
        width: 0.,
        active_color: color,
        inactive_color: color,
        urgent_color: color,
        active_gradient: None,
        inactive_gradient: None,
        urgent_gradient: None,
    }
}

/// The thumbnails-strip active-workspace ring: a 3px border in the system
/// accent color (the shell theme's `-st-accent-color`).
fn thumbnail_indicator_config(accent: [u8; 3]) -> niri_config::FocusRing {
    let color = niri_config::Color::from_rgba8_unpremul(accent[0], accent[1], accent[2], 0xff);
    niri_config::FocusRing {
        off: false,
        width: thumbnails::INDICATOR_WIDTH,
        active_color: color,
        inactive_color: color,
        urgent_color: color,
        active_gradient: None,
        inactive_gradient: None,
        urgent_gradient: None,
    }
}
