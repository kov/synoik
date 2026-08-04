use std::cmp::min;
use std::iter::zip;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::utils::{
    CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use synoik_config::{CornerRadius, LayoutPart, WindowingMode};

use super::focus_ring::FocusRing;
use super::insert_hint_element::{InsertHintElement, InsertHintRenderElement};
use super::scrolling::{Column, ColumnWidth};
use super::shadow::Shadow;
use super::thumbnails::{self, Strip};
use super::tile::Tile;
use super::workspace::{
    accent_workspace_shadow_config, compute_working_area, OutputId, Workspace,
    WorkspaceAddWindowTarget, WorkspaceId, WorkspaceRenderElement,
};
use super::{compute_overview_zoom, ActivateWindow, HitType, LayoutElement, Options};
use crate::animation::{Animation, Clock};
use crate::gnome::EdgeTileTarget;
use crate::input::swipe_tracker::SwipeTracker;
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::SolidColorRenderElement;
use crate::render_helpers::vulkan::VkTexture;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::rubber_band::RubberBand;
use crate::synoik_render_elements;
use crate::ui::overview_layout::{self, ControlsLayout};
use crate::utils::transaction::Transaction;
use crate::utils::{
    output_size, round_logical_in_physical, round_logical_in_physical_max1, ResizeEdge,
};
use crate::wallpaper::Wallpaper;

/// The fewest workspaces a monitor ever has, gnome-shell's `MIN_NUM_WORKSPACES`
/// (`js/ui/windowManager.js:42`, enforced at `:273-276` and `:286`): a fresh session
/// shows two desktops in the overview, not one.
pub const MIN_NUM_WORKSPACES: usize = 2;

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

/// gnome-shell's `WORKSPACE_MIN_SPACING` / `WORKSPACE_MAX_SPACING`
/// (`workspacesView.js:22-23`), the clamp on the overview row's inter-workspace
/// gap.
const WORKSPACE_MIN_SPACING: f64 = 24.;
const WORKSPACE_MAX_SPACING: f64 = 80.;

/// gnome-shell's `BACKGROUND_CORNER_RADIUS_PIXELS` (`workspace.js:30`), kept in
/// sync with `.workspace-background`'s `border-radius` (`_window-picker.scss:58`).
const WORKSPACE_BACKGROUND_CORNER_RADIUS: f64 = 30.;

/// The preview height GNOME's flat 30px radius is *for*: the window picker's preview on
/// the reference canvas ([`crate::ui::overview_layout::chrome_ramp`]), which is a hair
/// over half its 800px height. Below it the radius follows the preview down.
const REFERENCE_PREVIEW_H: f64 = 520.;

/// …and never below this, so a corner stays a corner (the interactive-floor rule).
const MIN_WORKSPACE_BACKGROUND_CORNER_RADIUS: f64 = 8.;

/// How far past its band a thumbnail's shadow is allowed to reach, so the active
/// workspace's accent glow is not cut flat at the thumbnail's top and bottom edges.
/// Comfortably more than the glow's own extent (softness + spread).
///
/// **Adaptive chrome, rule 1 — ramped.**
const SHADOW_GLOW_MARGIN: f64 = 48.;

/// A crop rect that crops nothing. `CropRenderElement` has no "no bounds" mode, so callers that
/// only want the wrapper for its type pass this; the element still has to survive the intersection,
/// hence half of `i32::MAX` on each side rather than the full range (which would overflow).
fn uncropped() -> Rectangle<i32, smithay::utils::Physical> {
    Rectangle::new(
        Point::from((-i32::MAX / 2, -i32::MAX / 2)),
        Size::from((i32::MAX, i32::MAX)),
    )
}

/// gnome-shell's `WORKSPACE_INACTIVE_SCALE` (`workspacesView.js:25`): how far a
/// workspace shrinks once the row has scrolled off it.
pub const WORKSPACE_INACTIVE_SCALE: f64 = 0.94;

/// How gnome-shell's overview arranges the workspace row (`FitMode`,
/// `workspacesView.js:85-88`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FitMode {
    /// One workspace centered, its neighbors peeking off the screen edges — the
    /// desktop and window-picker arrangement.
    Single,
    /// Every workspace fitted side by side, the row centered as a whole — the
    /// app-grid arrangement.
    All,
}

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
    /// The system accent color, kept so [`Self::active_shadow`] can be rebuilt whenever it
    /// or the view size changes.
    accent_color: [u8; 3],
    /// The **active** workspace's shadow: the workspace shadow every other workspace casts
    /// (`Workspace::render_shadow`), in the system accent color. Monitor-owned rather than
    /// per-workspace because only one workspace wears it at a time, and both rows — the
    /// small row and the window picker — draw it from here, so the cue cannot differ
    /// between them. See [`workspace::accent_workspace_shadow_config`].
    active_shadow: Shadow,
    /// The strip's new-workspace drop placeholder pill (gnome-shell's
    /// `.placeholder`).
    thumb_placeholder: FocusRing,
    /// A thumbnail being dragged along the strip to reorder the workspaces.
    thumb_drag: Option<ThumbDrag>,
    /// The strip closing the gap left by a workspace you dismissed — see
    /// [`Self::close_workspace`].
    thumb_close_slide: Option<CloseSlide>,
    /// Whether the overview is open.
    pub(super) overview_open: bool,
    /// Progress of the overview zoom animation, 1 is fully in overview.
    overview_progress: Option<OverviewProgress>,
    /// gnome-shell's `ControlsState` show-apps fraction (0 = window picker, 1 = app
    /// grid): eased when the show-apps state flips, shrinking the picker box and
    /// sliding the app grid up (`overviewControls.js` state adjustment). The target.
    app_grid_shown: bool,
    /// The ease driving [`Self::app_grid_shown`].
    app_grid_expand: Option<Animation>,
    /// Clock for driving animations.
    pub(super) clock: Clock,
    /// Configurable properties of the layout as received from the parent layout.
    pub(super) base_options: Rc<Options>,
    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
    /// Layout config overrides for this monitor.
    layout_config: Option<synoik_config::LayoutPart>,
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

/// A workspace thumbnail being dragged along the overview strip.
///
/// **Divergence (approved 2026-07-28).** gnome-shell's thumbnails do not reorder — a drag
/// on that strip is only ever a *window* being moved to another workspace, which we keep.
/// This adds macOS Mission Control's other gesture on top: grab the thumbnail itself and
/// the workspaces reorder. The two never collide, because they are told apart by what the
/// press landed on.
#[derive(Debug, Clone, Copy)]
struct ThumbDrag {
    /// The workspace the drag picked up.
    from: usize,
    /// Where in the thumbnail the pointer grabbed it, so it doesn't jump on the
    /// first motion.
    grab_offset: f64,
    /// The pointer's current position, in view coordinates.
    pos: Point<f64, Logical>,
}

/// The strip closing the gap a dismissed workspace left behind.
///
/// The workspace is gone from the model the instant it is closed — nothing may keep a
/// removed workspace alive for an animation's sake, or every index in the layout is a lie
/// for 250ms. What is animated is only *where the survivors are drawn*: the row is laid out
/// twice, once at the old count and once at the new, and the thumbnails ease between the
/// two. A workspace vanishing with no motion reads as a glitch rather than as something
/// you did.
#[derive(Debug)]
struct CloseSlide {
    /// The index the closed workspace occupied, so the old row can be reconstructed.
    removed_idx: usize,
    /// Whether the active workspace was below it, i.e. whether the old row's focus index
    /// was one higher than the current one.
    active_was_below: bool,
    /// 0 = the row as it stood with the workspace still in it, 1 = closed up.
    anim: Animation,
}

/// The index an armed drag would drop at: how many of the *other* thumbnails the dragged
/// one's center has passed. Taken against the strip at rest, so the row parting out of the
/// way underneath cannot feed back into the answer and make the target oscillate.
fn thumb_drag_target(strip: &Strip, drag: ThumbDrag) -> usize {
    let width = strip.thumbs[drag.from].size.w;
    let center = drag.pos.x - drag.grab_offset + width / 2.;
    strip
        .thumbs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != drag.from)
        .take_while(|(_, rect)| rect.loc.x + rect.size.w / 2. < center)
        .count()
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

synoik_render_elements! {
    MonitorInnerRenderElement => {
        Workspace = CropRenderElement<WorkspaceRenderElement>,
        // The insert hint, and the thumbnail-strip rings clipped to their band.
        InsertHint = CropRenderElement<InsertHintRenderElement>,
        // Insert hint between workspaces, and the thumbnail-strip indicator.
        Ring = InsertHintRenderElement,
        Shadow = ShadowRenderElement,
        CroppedShadow = CropRenderElement<ShadowRenderElement>,
        SolidColor = SolidColorRenderElement,
        CroppedSolidColor = CropRenderElement<SolidColorRenderElement>,
        // The wallpaper in a workspace thumbnail.
        RoundedTexture = RoundedTextureRenderElement<VkTexture>,
        CroppedRoundedTexture = CropRenderElement<RoundedTextureRenderElement<VkTexture>>,
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

    fn animate_from(&mut self, from: f64, clock: Clock, config: synoik_config::Animation) {
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

    fn is_animation(&self) -> bool {
        matches!(self, OverviewProgress::Animation(_))
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

        // Prepare the workspaces: set output, empty last, at least MIN_NUM_WORKSPACES.
        let mut active_workspace_idx = 0;

        for (idx, ws) in workspaces.iter_mut().enumerate() {
            assert!(ws.has_windows_or_name());

            ws.set_output(Some(output.clone()));
            ws.update_config(options.clone());

            if ws_id_to_activate.is_some_and(|id| ws.id() == id) {
                active_workspace_idx = idx;
            }
        }

        // The trailing empty workspace, then gnome-shell's minimum on top of it, in that
        // order: a fresh monitor comes up showing two desktops
        // (`windowManager.js:267-276`).
        let new_workspace = || Workspace::new(output.clone(), clock.clone(), options.clone());
        if workspaces
            .last()
            .is_none_or(|ws: &Workspace<W>| ws.has_windows_or_name())
        {
            workspaces.push(new_workspace());
        }
        while workspaces.len() < MIN_NUM_WORKSPACES {
            workspaces.push(new_workspace());
        }

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
            accent_color: crate::gnome::ACCENT_BLUE,
            active_shadow: Shadow::new(accent_workspace_shadow_config(
                options.overview.workspace_shadow,
                view_size,
                crate::gnome::ACCENT_BLUE,
            )),
            thumb_placeholder: FocusRing::new(thumbnail_placeholder_config()),
            thumb_drag: None,
            thumb_close_slide: None,
            insert_hint_render_loc: None,
            overview_open: false,
            overview_progress: None,
            app_grid_shown: false,
            app_grid_expand: None,
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

    /// The workspace at this index on the monitor, which is also its index along the
    /// overview thumbnails strip.
    pub fn workspace_at(&self, idx: usize) -> Option<&Workspace<W>> {
        self.workspaces.get(idx)
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
        config: Option<synoik_config::Animation>,
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

    pub fn add_column(&mut self, workspace_idx: usize, column: Column<W>, activate: bool) {
        let workspace = &mut self.workspaces[workspace_idx];

        workspace.add_column(column, activate);

        // After adding a new window, workspace becomes this output's own.
        if workspace.name().is_none() {
            workspace.original_output = OutputId::new(&self.output);
        }

        if workspace_idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
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
        let (workspace_idx, target) = self.resolve_add_window_target(target);

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

    /// Restores the workspace-list invariants: the last workspace is empty and unnamed,
    /// and there are at least [`MIN_NUM_WORKSPACES`] of them.
    ///
    /// **Divergence (approved 2026-08-03).** gnome-shell's `_checkWorkspaces`
    /// (`js/ui/windowManager.js:278-291`) also *reaps* every empty non-active workspace
    /// but the last, so emptying a desktop makes it vanish under you. We keep empties
    /// around and let you dismiss them by hand, macOS-style — see
    /// [`Self::workspace_is_closable`] and `docs/fork/dynamic-workspaces-divergence.md`.
    /// Everything the reaper *maintained* still holds; only the reaping is gone.
    pub fn clean_up_workspaces(&mut self) {
        assert!(self.workspace_switch.is_none());

        // "If we don't have an empty workspace at the end, add one", then "enforce minimum
        // number of workspaces" (`windowManager.js:267-276`).
        if self.workspaces.last().unwrap().has_windows_or_name() {
            self.add_workspace_bottom();
        }
        while self.workspaces.len() < MIN_NUM_WORKSPACES {
            self.add_workspace_bottom();
        }
    }

    /// Every closable workspace's close-button rect, in view coordinates — the strip's
    /// hover chrome and the hit test read the same list, so a button you can see is a
    /// button you can press. See [`Self::workspace_is_closable`].
    ///
    /// At rest (the only time this is asked: the buttons are hover-driven and the pointer
    /// is not aiming at anything mid-slide), so no slide offset is applied.
    pub fn thumbnail_close_rects(&self) -> Vec<(WorkspaceId, Rectangle<f64, Logical>)> {
        let Some(strip) = self.thumbnail_strip() else {
            return Vec::new();
        };
        let ramp = overview_layout::chrome_ramp(self.view_size);
        zip(&self.workspaces, &strip.thumbs)
            .enumerate()
            .filter(|(idx, _)| self.workspace_is_closable(*idx))
            .map(|(idx, (ws, slot))| {
                let thumb =
                    thumbnail_drawn_rect(*slot, self.workspace_render_scale(idx), Point::default());
                (ws.id(), thumbnails::close_rect(thumb, ramp))
            })
            .collect()
    }

    /// The workspace whose thumbnail close button is under the position.
    pub fn thumbnail_close_under(&self, pos: Point<f64, Logical>) -> Option<WorkspaceId> {
        // Clipped to the band like everything else the strip draws: a button scrolled out
        // of the band is not drawn, so it is not there to aim at either.
        let strip = self.thumbnail_strip()?;
        if !strip.band.contains(pos) {
            return None;
        }
        self.thumbnail_close_rects()
            .into_iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(id, _)| id)
    }

    /// Closes the workspace with this id, if it is closable. Returns whether it went.
    ///
    /// The remaining workspaces slide up into the gap: `move_workspace_to_idx`-style
    /// reordering is what the strip already animates, and a workspace vanishing with no
    /// motion reads as a glitch rather than as something you did.
    pub fn close_workspace(&mut self, id: WorkspaceId) -> bool {
        let Some(idx) = self.workspaces.iter().position(|ws| ws.id() == id) else {
            return false;
        };
        if !self.workspace_is_closable(idx) {
            return false;
        }

        // A switch in flight indexes workspaces by position, so it cannot survive one
        // being spliced out from under it.
        self.workspace_switch = None;
        self.workspaces.remove(idx);
        let active_was_below = self.active_workspace_idx > idx;
        if active_was_below {
            self.active_workspace_idx -= 1;
        }
        self.clean_up_workspaces();

        // gnome-shell's own strip transitions run on `SIDE_CONTROLS_ANIMATION_TIME`
        // (250ms, EASE_OUT_QUAD, `overviewControls.js:360-366`) — a fixed duration, not the
        // configurable overview open/close one. Only whether animations run at all is
        // inherited from the config.
        let config = synoik_config::Animation {
            off: self.options.animations.overview_open_close.0.off,
            kind: synoik_config::animations::Kind::Easing(
                synoik_config::animations::EasingParams {
                    duration_ms: 250,
                    curve: synoik_config::animations::Curve::EaseOutQuad,
                },
            ),
        };
        self.thumb_close_slide = Some(CloseSlide {
            removed_idx: idx,
            active_was_below,
            anim: Animation::new(self.clock.clone(), 0., 1., 0., config),
        });
        true
    }

    /// How many workspaces this monitor has, never fewer than [`MIN_NUM_WORKSPACES`].
    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    /// Whether this workspace may be closed from its thumbnail: it has to be windowless
    /// (naming one means you want it kept), it can't be the trailing empty one — closing
    /// that would just re-append it — and closing it can't take us below
    /// [`MIN_NUM_WORKSPACES`].
    pub fn workspace_is_closable(&self, idx: usize) -> bool {
        idx + 1 < self.workspaces.len()
            && self.workspaces.len() > MIN_NUM_WORKSPACES
            && !self.workspaces[idx].has_windows_or_name()
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

    pub fn remove_workspace_by_idx(&mut self, idx: usize) -> Workspace<W> {
        if idx == self.workspaces.len() - 1 {
            self.add_workspace_bottom();
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

        self.update_app_grid_expand();
    }

    pub(super) fn are_animations_ongoing(&self) -> bool {
        self.workspace_switch
            .as_ref()
            .is_some_and(|s| s.is_animation_ongoing())
            || self.app_grid_expand.is_some()
            || self.thumb_close_slide.is_some()
            || self.workspaces.iter().any(|ws| ws.are_animations_ongoing())
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.workspace_switch.is_some()
            || self.app_grid_expand.is_some()
            || self.thumb_close_slide.is_some()
            || self
                .workspaces
                .iter()
                .any(|ws| ws.are_transitions_ongoing())
    }

    /// The corner radius the workspace background is rounded to in the overview,
    /// in **pre-zoom workspace units** — the row is scaled by the zoom afterwards,
    /// so dividing it out here is what lands the radius at 30 physical px.
    ///
    /// gnome-shell's `.workspace-background` is `border-radius: 30px` with a
    /// matching `box-shadow` on the same box (`_window-picker.scss:56-60`), lerped
    /// 0 → 30 on the workspace's state adjustment
    /// (`WorkspaceBackground._updateBorderRadius`, `workspace.js:1002-1009`).
    ///
    /// One accessor because the wallpaper and the shadow both need it and must
    /// agree: they were derived separately, the shadow's stayed square, and the
    /// backdrop showed through each rounded corner as a pointy dark tab.
    pub fn workspace_background_radius(&self) -> f64 {
        let progress = self.expose_progress().unwrap_or(0.);
        let zoom = self.overview_zoom();
        // **Adaptive chrome, rule 1 — self-derived** (`docs/fork/adaptive-overview-chrome.md`).
        // The preview's box already scales with the canvas, so its corner is a fraction of
        // its own height rather than a flat 30: 30px on a 200px-tall preview is a lozenge,
        // not a rounded rectangle. Capped at GNOME's constant, so every canvas at or above
        // the reference is unchanged, and floored so the corner never disappears.
        let preview_h = self.view_size.h * zoom;
        let radius = (WORKSPACE_BACKGROUND_CORNER_RADIUS * preview_h / REFERENCE_PREVIEW_H).clamp(
            MIN_WORKSPACE_BACKGROUND_CORNER_RADIUS,
            WORKSPACE_BACKGROUND_CORNER_RADIUS,
        );
        radius * progress / zoom
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        // The active workspace's accent shadow, baked exactly as each workspace bakes its
        // own (`Workspace::update_render_elements`) — same size, same radius — so the two
        // are interchangeable and both rows can draw either through the same transform.
        // Rebuilt every frame because the radius rides the overview's opening ramp and the
        // accent color can change under us at any time.
        self.active_shadow
            .update_config(accent_workspace_shadow_config(
                self.options.overview.workspace_shadow,
                self.view_size,
                self.accent_color,
            ));
        self.active_shadow.update_render_elements(
            self.view_size,
            true,
            CornerRadius::from(self.workspace_background_radius() as f32),
            self.scale.fractional_scale(),
            1.,
        );

        if let Some(strip) = self.thumbnail_strip() {
            let scale = self.scale.fractional_scale();

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

        let insert_hint_ws_id = self
            .insert_hint
            .as_ref()
            .and_then(|hint| hint.workspace.existing_id());

        // Deliberately NOT the culled iteration. A drop onto the thumbnail strip names a workspace
        // by index (`insert_position`), so the hint can point at a workspace that is scrolled out
        // of the overview and therefore culled from rendering — and it still needs its geometry.
        // Harvesting this inside the culled loop below is what made that case unwrap a None.
        let insert_hint_ws_geo = insert_hint_ws_id.and_then(|id| {
            zip(self.workspaces.iter(), self.workspaces_render_geo())
                .find(|(ws, _)| ws.id() == id)
                .map(|(_, geo)| geo)
        });

        let background_radius = self.workspace_background_radius();
        for (ws, _) in self.workspaces_with_render_geo_mut(true) {
            ws.update_render_elements(is_active, background_radius);
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
                            let clamp_to = insert_hint_ws_geo
                                .filter(|_| matches!(hint.position, InsertPosition::NewColumn(_)));
                            if let Some(geo) = clamp_to {
                                let zoom = self.overview_zoom();
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
                    // Fit-single: dropping into a new workspace is a window-picker
                    // gesture, and the app grid is not a drop target.
                    let gap = self.workspace_gap(zoom, FitMode::Single);

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

        for ws in &mut self.workspaces {
            ws.update_config(options.clone());
        }

        self.insert_hint_element
            .update_config(options.layout.insert_hint);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(
        &mut self,
        layout_config: Option<synoik_config::LayoutPart>,
    ) -> bool {
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
        let new_idx = min(self.active_workspace_idx + 1, self.workspaces.len() - 1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if new_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
        }

        let previous_workspace_id = self.previous_workspace_id;
        self.activate_workspace(new_idx);
        self.workspace_switch = None;
        self.previous_workspace_id = previous_workspace_id;

        self.clean_up_workspaces();
    }

    pub fn move_workspace_up(&mut self) {
        let new_idx = self.active_workspace_idx.saturating_sub(1);
        if new_idx == self.active_workspace_idx {
            return;
        }

        self.workspaces.swap(self.active_workspace_idx, new_idx);

        if self.active_workspace_idx == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
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

        let new_idx = new_idx.clamp(0, self.workspaces.len() - 1);
        if old_idx == new_idx {
            return;
        }

        let ws = self.workspaces.remove(old_idx);
        self.workspaces.insert(new_idx, ws);

        // Whichever index ended up last, the trailing workspace must stay empty.
        let moved_into_last = if new_idx > old_idx { new_idx } else { old_idx };
        if moved_into_last == self.workspaces.len() - 1 {
            // Insert a new empty workspace.
            self.add_workspace_bottom();
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

    /// The gap between two workspaces at a given [`FitMode`] — gnome-shell's
    /// `WorkspacesView._getSpacing` (`workspacesView.js:207-226`):
    /// `(availableSpace - workspaceSize * 0.4) * (1 - fitMode)`, clamped to
    /// `WORKSPACE_MIN_SPACING`..`WORKSPACE_MAX_SPACING` (24..80).
    ///
    /// At [`FitMode::Single`] and the window picker's zoom the workspace takes
    /// most of the width, so the raw value goes negative and clamps to the
    /// minimum — which is the point: the side margins stay free so the neighbor
    /// workspaces peek in at the screen edges. The app grid's much smaller zoom
    /// would instead run the formula up to the maximum; there the
    /// [`FitMode::All`] `(1 - fitMode)` factor zeroes it, so the fitted row packs
    /// back at the minimum.
    /// [`Self::workspace_gap`] at the picker's own zoom — a probe for the conformance
    /// corpus, which asserts the ramped clamps rather than the raw formula.
    #[cfg(test)]
    pub fn workspace_gap_for_test(&self) -> f64 {
        self.workspace_gap(self.overview_zoom(), FitMode::Single)
    }

    fn workspace_gap(&self, zoom: f64, fit_mode: FitMode) -> f64 {
        let scale = self.scale.fractional_scale();
        let gap = if self.workspaces_horizontal() {
            let ws_width = self.view_size.w * zoom;
            let available = (self.view_size.w - ws_width) / 2.;
            let raw = match fit_mode {
                FitMode::Single => available - ws_width * 0.4,
                FitMode::All => 0.,
            };
            // **Adaptive chrome, rule 2 — ramped.** The clamps are fixed logical
            // constants, and on a small canvas the formula runs straight into the 80px
            // maximum, which is where "comical" spacing came from.
            let ramp = crate::ui::overview_layout::chrome_ramp(self.view_size);
            raw.clamp(WORKSPACE_MIN_SPACING * ramp, WORKSPACE_MAX_SPACING * ramp)
        } else {
            self.view_size.h * 0.1 * zoom
        };
        round_logical_in_physical_max1(scale, gap)
    }

    fn workspace_size_with_gap(&self, zoom: f64) -> Size<f64, Logical> {
        let gap = self.workspace_gap(zoom, FitMode::Single);
        if self.workspaces_horizontal() {
            self.workspace_size(zoom) + Size::from((gap, 0.))
        } else {
            self.workspace_size(zoom) + Size::from((0., gap))
        }
    }

    /// The strip-axis extent of one workspace plus the fit-single gap.
    fn workspace_extent_with_gap(&self, zoom: f64) -> f64 {
        let size = self.workspace_size_with_gap(zoom);
        if self.workspaces_horizontal() {
            size.w
        } else {
            size.h
        }
    }

    /// Where we sit on gnome-shell's state axis: 0 `HIDDEN`, 1 `WINDOW_PICKER`,
    /// 2 `APP_GRID` (`_stateAdjustment`, `overviewControls.js:278-308`).
    ///
    /// gnome-shell carries exactly one adjustment over that range and derives the
    /// fit mode, the workspaces box and the app-display box from it, so hiding
    /// from the app grid travels 2 → 0 *through* `WINDOW_PICKER` and every blend
    /// unwinds in order. We carry two scalars — how far the overview is open and
    /// how far the app grid is in — and reconstruct the axis here: the show-apps
    /// fraction is a *second unit* on top of the first, so it is scaled by the
    /// open progress. A close from the grid freezes the show-apps scalar and runs
    /// the open one down, which lands here as the 2 → 0 sweep.
    ///
    /// Every state-dependent blend must go through this rather than through the
    /// raw open progress, or the two scalars can describe a state the reference
    /// never passes through — e.g. an app-grid row at a near-desktop zoom.
    fn overview_state(&self) -> f64 {
        let progress = self
            .overview_progress
            .as_ref()
            .map_or(0., |p| p.clamped_value())
            .clamp(0., 1.);
        progress * (1. + self.app_grid_fraction())
    }

    /// [`Self::overview_state`] for instrumentation: `None` when the overview is
    /// closed, so a log line can say "overview 1.42" or nothing at all.
    pub fn overview_state_value(&self) -> Option<f64> {
        let state = self.overview_state();
        (state > 0.).then_some(state)
    }

    /// The `HIDDEN` → `WINDOW_PICKER` leg of [`Self::overview_state`], which is what the
    /// zoom and the exposé spread ride: 0 on the desktop, 1 at the picker, and **1 for the
    /// whole app-grid leg above it**.
    ///
    /// That saturation is the ordering gnome-shell's single adjustment gives for free. A
    /// close from the app grid travels 2 → 0 *through* `WINDOW_PICKER`, so the grid goes
    /// away first, at a parked zoom, and only then does the overview zoom into the active
    /// workspace. Blending both at once instead — which is what the raw progress does —
    /// runs the entire return-to-the-desktop behind an app grid that is still on screen.
    ///
    /// With no app grid in play this is the raw progress, **overshoot included**: the open
    /// is a spring, and clamping it here would quietly flatten the bounce.
    /// [`Self::overview_state`] must stay clamped instead — an overshoot past 1 there would
    /// read as "starting to show the apps".
    fn open_fraction(&self) -> f64 {
        if self.app_grid_fraction() > 0. {
            self.overview_state().min(1.)
        } else {
            self.overview_progress.as_ref().map_or(0., |p| p.value())
        }
    }

    /// Where the row currently sits on the workspace axis — gnome-shell's
    /// `_scrollAdjustment.value`, which eases to the active index over a switch
    /// (`WorkspacesView._scrollToActive`, `workspacesView.js:441-455`) and is
    /// what `_updateWorkspacesState` measures each workspace's distance from.
    ///
    /// Deliberately *not* [`Self::workspace_render_idx`]: that one carries a
    /// correction term for a switch synchronized with the overview animation, so
    /// it is a row offset rather than a workspace index.
    fn workspace_scroll_position(&self) -> f64 {
        match &self.workspace_switch {
            Some(switch) => switch.current_idx(),
            None => self.active_workspace_idx as f64,
        }
    }

    /// How far the inactive-workspace shrink is faded in — see
    /// [`workspace_render_scale`] for why it is ramped rather than constant.
    fn workspace_inactive_ramp(&self) -> f64 {
        if self.workspaces_horizontal() {
            self.expose_progress().unwrap_or(0.)
        } else {
            0.
        }
    }

    /// This monitor's [`workspace_render_scale`] for one workspace.
    pub fn workspace_render_scale(&self, idx: usize) -> f64 {
        workspace_render_scale(
            self.workspace_scroll_position(),
            idx,
            self.workspace_inactive_ramp(),
        )
    }

    /// Where workspace 0 starts on the strip axis, and how far apart consecutive
    /// workspaces sit — both relative to the centered slot
    /// [`Self::workspaces_static_offset`] places.
    ///
    /// This is gnome-shell's fit-single row (`_getFirstFitSingleWorkspaceBox`,
    /// `workspacesView.js:171-204`): the whole row slides so the active workspace lands on
    /// the centered slot.
    ///
    /// **Divergence (approved 2026-08-03).** `WorkspacesView.vfunc_allocate`
    /// (`workspacesView.js:330-388`) also walks a *fit-all* row and lerps the two on the
    /// app-grid state, which is how gnome-shell's picker becomes its app-grid row. Ours
    /// never makes that trip: the small row is drawn in both states by
    /// [`Self::thumbnail_strip`], and the picker fades away over it instead. So the picker
    /// is fit-single always, and the fit-all row lives with the row that actually uses it
    /// ([`thumbnails::strip_geometry`]).
    fn workspaces_strip_axis(&self, zoom: f64) -> (f64, f64) {
        let render_idx = self.workspace_render_idx();

        if !self.workspaces_horizontal() {
            let extent = self.workspace_extent_with_gap(zoom);
            return (-render_idx * extent, extent);
        }

        let view_w = self.view_size.w;
        // Expressed against the slot the drawn size is centered on.
        let slot = (view_w - self.workspace_size(zoom).w) / 2.;
        let (x, extent) = fit_single_row(
            view_w,
            self.workspace_size(zoom).w,
            self.workspace_gap(zoom, FitMode::Single),
            render_idx,
        );
        (x - slot, extent)
    }

    /// The allocated box of every piece of overview chrome on this monitor
    /// (gnome-shell's `ControlsManagerLayout`). One place decides where the
    /// search entry, workspace row, dash and window picker go; everything else
    /// consumes these boxes.
    pub fn controls_layout(&self) -> ControlsLayout {
        // WINDOW_PICKER (1) → APP_GRID (2) as the show-apps leg eases in. Below
        // the picker the chrome keeps its picker layout and the *zoom* blends it
        // toward the desktop, which is how gnome-shell's HIDDEN box relates to the
        // WINDOW_PICKER one (`overviewControls.js:207-216`).
        self.controls_layout_at(overview_layout::state::WINDOW_PICKER + self.app_grid_leg())
    }

    /// The `WINDOW_PICKER` → `APP_GRID` leg of [`Self::overview_state`]: 0 at the picker,
    /// 1 at the grid.
    ///
    /// **This, not [`Self::app_grid_fraction`], is what every app-grid blend consumes** —
    /// the grid's own opacity and box, the picker's fade behind it, and the inertness of
    /// the previews. The raw scalar is *frozen* across a close (the overview must not
    /// animate the grid shut and close at the same time), so reading it directly leaves the
    /// grid "fully in" for the whole close: the picker stays at alpha 0 and the return to
    /// the desktop happens invisibly behind it. Going through the state axis is what makes
    /// the close unwind in gnome-shell's order — see [`Self::open_fraction`].
    pub fn app_grid_leg(&self) -> f64 {
        (self.overview_state() - overview_layout::state::WINDOW_PICKER).clamp(0., 1.)
    }

    /// The same chrome layout at an arbitrary point on gnome-shell's state axis
    /// — how the geometry of an *endpoint* state is asked for while a transition
    /// is in flight (`getWorkspacesBoxForState`, `overviewControls.js:196-215`).
    fn controls_layout_at(&self, state: f64) -> ControlsLayout {
        let entry_w =
            crate::ui::overview_search::entry_width(overview_layout::chrome_ramp(self.view_size));
        overview_layout::layout(
            self.view_size,
            self.working_area.loc.y,
            overview_layout::Measured {
                search_entry_height: crate::ui::overview_search::PREFERRED_ENTRY_HEIGHT,
                search_entry_width: entry_w,
                search_entry_mid_y: crate::ui::overview_search::ENTRY_CONTROL_MID_Y,
                dash_preferred_height: crate::ui::dash::preferred_height(self.view_size),
            },
            state,
        )
    }

    /// Retires the show-apps and close-slide eases when they land.
    fn update_app_grid_expand(&mut self) {
        if self.app_grid_expand.as_ref().is_some_and(|a| a.is_done()) {
            self.app_grid_expand = None;
        }
        if self
            .thumb_close_slide
            .as_ref()
            .is_some_and(|s| s.anim.is_done())
        {
            self.thumb_close_slide = None;
        }
    }

    /// The show-apps state fraction (0 = window picker, 1 = app grid), eased.
    pub fn app_grid_fraction(&self) -> f64 {
        match &self.app_grid_expand {
            Some(anim) => anim.clamped_value().clamp(0., 1.),
            None => {
                if self.app_grid_shown {
                    1.
                } else {
                    0.
                }
            }
        }
    }

    /// Ease the app grid in or out. gnome-shell drives the show-apps state
    /// adjustment with `EASE_OUT_SINE` over `SIDE_CONTROLS_ANIMATION_TIME` (250ms,
    /// `overviewControls.js:654-657`); our `Curve` has no sine, so the equivalent
    /// cubic-bézier reproduces it. Only whether animations run is inherited from the
    /// config (like [`Self::update_app_grid_expand`]).
    pub(super) fn set_app_grid(&mut self, shown: bool) {
        if shown == self.app_grid_shown {
            return;
        }
        let from = self.app_grid_fraction();
        self.app_grid_shown = shown;
        let config = synoik_config::Animation {
            off: self.options.animations.overview_open_close.0.off,
            kind: synoik_config::animations::Kind::Easing(
                synoik_config::animations::EasingParams {
                    duration_ms: 250,
                    curve: synoik_config::animations::Curve::CubicBezier(0.39, 0.575, 0.565, 1.),
                },
            ),
        };
        self.app_grid_expand = Some(Animation::new(
            self.clock.clone(),
            from,
            if shown { 1. } else { 0. },
            0.,
            config,
        ));
    }

    /// Snap the app grid back to the window picker with no animation — for when the
    /// overview is fully hidden (the close deliberately froze the state) or freshly
    /// entered, so it always opens in the picker.
    pub(super) fn reset_app_grid(&mut self) {
        self.app_grid_shown = false;
        self.app_grid_expand = None;
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
        if self.options.layout.windowing_mode == WindowingMode::Floating {
            // The open leg of the state axis, so that closing from the app grid
            // re-fits the row first and zooms up after, rather than doing both at
            // once through states the reference never visits.
            return self.zoom_at(Some(self.open_fraction()));
        }
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
            // The same open leg the zoom rides, so the row's box and its size stay
            // consistent all the way down a close from the app grid.
            self.controls_layout().workspaces.loc.y * self.open_fraction()
        } else {
            (self.view_size.h - ws_size.h) / 2.
        };

        let scale = self.scale.fractional_scale();
        Point::from((x, y))
            .to_physical_precise_round(scale)
            .to_logical(scale)
    }

    /// Whether the overview is still *animating open* on this monitor.
    ///
    /// gnome-shell's overlay-key handler asks the same question of its state
    /// adjustment — `transitioning && finalState > initialState`
    /// (`overviewControls.js:426-433`) — to tell a second Super tap that lands
    /// mid-open (shift a state up, into the app grid) from one that lands after it
    /// settled (toggle the overview back shut).
    pub fn is_overview_opening(&self) -> bool {
        let Some(progress) = &self.overview_progress else {
            return false;
        };
        progress.is_animation() && self.overview_open && progress.clamped_value() < 1.
    }

    /// In GNOME windowing mode, the overview spreads each workspace's windows
    /// into picker slots (gnome-shell's window picker); this is how far along
    /// that spread is, if it's on at all.
    pub fn expose_progress(&self) -> Option<f64> {
        if self.options.layout.windowing_mode != WindowingMode::Floating {
            return None;
        }
        self.overview_progress.as_ref()?;
        // The `HIDDEN` → `WINDOW_PICKER` leg, so the spread is *parked* across the app-grid
        // leg rather than unwinding underneath it: a close from the grid must not collapse
        // the previews back to their desktop positions while the grid is still on screen.
        let progress = self.open_fraction().clamp(0., 1.);
        (progress > 0.).then_some(progress)
    }

    /// Whether the overview draws the small workspace row.
    ///
    /// **Divergence (approved 2026-08-03).** gnome-shell hides its strip below
    /// `NUM_WORKSPACES_THRESHOLD + 1` workspaces (`workspaceThumbnail.js:697-706`) and
    /// eases an `expandFraction` when the count crosses it. We always show it, whatever the
    /// count and in both overview states: the row is the desktop switcher, and one that
    /// appears and disappears under you is not one you can aim at. See
    /// `docs/fork/dynamic-workspaces-divergence.md`.
    pub fn thumbnails_visible(&self) -> bool {
        self.expose_progress().is_some()
    }

    /// The thumbnails strip, laid out in the band [`Self::controls_layout`]
    /// allocates it. While a drag hovers one of its gaps, the strip makes room
    /// for the new-workspace drop placeholder there; while a *thumbnail* is
    /// being dragged, the row parts around where it would land.
    pub fn thumbnail_strip(&self) -> Option<Strip> {
        let strip = self.thumbnail_strip_at_rest()?;
        Some(self.apply_thumb_drag(strip))
    }

    /// The strip as laid out with no reorder drag applied. Every drag
    /// computation works off this, so the row it re-lays does not feed back into
    /// where the drag thinks the slots are.
    fn thumbnail_strip_at_rest(&self) -> Option<Strip> {
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
        let band = self.controls_layout().workspace_row;
        let zoom = band.size.h / self.view_size.h;
        let thumb_w = (self.view_size.w * zoom).round();
        let gap = self.workspace_gap(zoom, FitMode::All);
        let lay = |n: usize, focus: f64| {
            thumbnails::strip_geometry(self.view_size, band, thumb_w, gap, n, placeholder, focus)
        };
        let strip = lay(self.workspaces.len(), self.workspace_render_idx());
        Some(self.apply_close_slide(strip, &lay))
    }

    /// Eases the row from where it stood with a just-closed workspace still in it to where
    /// it stands without — see [`CloseSlide`]. The survivors keep their identity
    /// (`thumbs[i]` is still `workspaces[i]`); only their positions are interpolated.
    fn apply_close_slide(&self, mut strip: Strip, lay: &dyn Fn(usize, f64) -> Strip) -> Strip {
        let Some(slide) = &self.thumb_close_slide else {
            return strip;
        };
        let t = slide.anim.clamped_value().clamp(0., 1.);

        // The row as it was: one more workspace, and the focus index shifted back up if the
        // closed one sat above the active workspace.
        let focus_before =
            self.workspace_render_idx() + f64::from(u8::from(slide.active_was_below));
        let before = lay(self.workspaces.len() + 1, focus_before);

        for (i, rect) in strip.thumbs.iter_mut().enumerate() {
            // Map this survivor back onto the slot it held in the old row.
            let was = if i < slide.removed_idx { i } else { i + 1 };
            let Some(from) = before.thumbs.get(was) else {
                continue;
            };
            rect.loc.x = from.loc.x + (rect.loc.x - from.loc.x) * t;
        }
        strip
    }

    /// Re-lays a strip around a reorder drag: the dragged thumbnail follows the
    /// pointer, and the others close up and part around the slot it would land
    /// in.
    ///
    /// The returned `thumbs` stay in **workspace order** (`thumbs[i]` belongs to
    /// `workspaces[i]`), which is what every consumer — rendering, `thumb_under`,
    /// `drop_target` — assumes.
    fn apply_thumb_drag(&self, mut strip: Strip) -> Strip {
        let Some(drag) = self.thumb_drag else {
            return strip;
        };
        if drag.from >= strip.thumbs.len() {
            return strip;
        }

        // Where it would land, and hence the order the others take.
        let target = thumb_drag_target(&strip, drag);
        let mut order: Vec<usize> = (0..strip.thumbs.len())
            .filter(|i| *i != drag.from)
            .collect();
        order.insert(target.min(order.len()), drag.from);

        let slots: Vec<_> = strip.thumbs.clone();
        for (slot, ws) in order.into_iter().enumerate() {
            strip.thumbs[ws].loc = slots[slot].loc;
        }
        // …and the dragged one is wherever the pointer holds it.
        strip.thumbs[drag.from].loc.x = drag.pos.x - drag.grab_offset;

        strip
    }

    /// Picks up the thumbnail at `idx` for reordering (**divergence**, see
    /// [`ThumbDrag`]). Returns whether there was a thumbnail there to pick up.
    pub fn begin_thumb_drag(&mut self, idx: usize, pos: Point<f64, Logical>) -> bool {
        let Some(strip) = self.thumbnail_strip_at_rest() else {
            return false;
        };
        let Some(rect) = strip.thumbs.get(idx) else {
            return false;
        };
        self.thumb_drag = Some(ThumbDrag {
            from: idx,
            grab_offset: pos.x - rect.loc.x,
            pos,
        });
        true
    }

    /// Follows the pointer.
    pub fn update_thumb_drag(&mut self, pos: Point<f64, Logical>) {
        if let Some(drag) = &mut self.thumb_drag {
            drag.pos = pos;
        }
    }

    /// Ends the drag, reordering the workspaces to where the thumbnail was
    /// dropped. Returns whether anything moved.
    pub fn finish_thumb_drag(&mut self) -> bool {
        let Some(drag) = self.thumb_drag.take() else {
            return false;
        };
        let Some(strip) = self.thumbnail_strip_at_rest() else {
            return false;
        };
        let target = thumb_drag_target(&strip, drag);
        if target == drag.from {
            return false;
        }
        self.move_workspace_to_idx(drag.from, target);
        true
    }

    /// Drops the drag without reordering (the overview closing under it, a
    /// cancelled grab).
    pub fn cancel_thumb_drag(&mut self) {
        self.thumb_drag = None;
    }

    /// Whether a reorder drag is under way.
    pub fn thumb_drag_active(&self) -> bool {
        self.thumb_drag.is_some()
    }

    /// The strip slides in from above the screen with the overview
    /// transition (gnome-shell translates its box in and out).
    ///
    /// Divergence: gnome-shell instead eases `expandFraction` into the box
    /// height when the strip appears mid-overview; we fold the expand into the
    /// same slide, so the strip slides down as the picker makes room for it.
    fn thumbnail_slide_offset(&self, strip: &Strip, progress: f64) -> f64 {
        let bounds = strip.bounds();
        let extent = bounds.loc.y + bounds.size.h + thumbnails::INDICATOR_WIDTH;
        -extent * (1. - progress)
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
        self.accent_color = accent;
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
        let (first_ws_pos, ws_extent_with_gap) = self.workspaces_strip_axis(zoom);

        let static_offset = self.workspaces_static_offset(zoom);

        let first_ws_pos = round_logical_in_physical(scale, first_ws_pos);

        // The *slot* keeps the full workspace size, so neither the row's advance
        // nor its centered anchor move; only the workspace drawn in it shrinks,
        // about the slot's center like gnome-shell's centered pivot.
        let ramp = self.workspace_inactive_ramp();
        let scroll_position = self.workspace_scroll_position();
        let view_size = self.view_size;

        // Return position for one-past-last workspace too.
        (0..=self.workspaces.len()).map(move |idx| {
            let pos = first_ws_pos + idx as f64 * ws_extent_with_gap;
            let loc = if horizontal {
                Point::from((pos, 0.))
            } else {
                Point::from((0., pos))
            };
            let loc = loc + static_offset;

            let ws_scale = workspace_render_scale(scroll_position, idx, ramp);
            let size = if ws_scale == 1. {
                ws_size
            } else {
                let size = view_size.upscale(zoom * ws_scale);
                size.to_physical_precise_ceil(scale).to_logical(scale)
            };
            let loc = loc + Point::from(((ws_size.w - size.w) / 2., (ws_size.h - size.h) / 2.));

            // Even though all components that go into loc are rounded to physical pixels, the
            // floating point addition may lose precision. This can result for example in the
            // current workspace having y = 0.0000000000002 and thus missing pointer hits at the
            // monitor edge with y = 0. So, post-round the location too.
            let loc = loc.to_physical_precise_round(scale).to_logical(scale);

            Rectangle::new(loc, size)
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

    /// Every window preview showing its picker overlay on this monitor: the
    /// window, where it draws in output coordinates, and how far the overlay has
    /// faded in (`showOverlay`, `windowPreview.js:310`).
    pub fn preview_overlays(&self) -> Vec<(W::Id, Rectangle<f64, Logical>, f64)> {
        // Overlays are a property of the state, not of the last pointer motion: opening the app
        // grid drops them even if the pointer never moves (`_syncOverlay`, `workspace.js:775-777`
        // — see `window_under`).
        if self.app_grid_leg() > 0. {
            return Vec::new();
        }
        // Only previews actually *showing* an overlay: two hit tests rely on that
        // (`preview_hover_under`, `preview_close_under`) to avoid arming a hover
        // from a button that is not drawn.
        let mut overlays = self.preview_rects();
        overlays.retain(|(_, _, hover)| *hover > 0.);
        overlays
    }

    /// The app icon's scale on the overview axis — `_updateIconScale`
    /// (`windowPreview.js:238-252`): `1 - |WINDOW_PICKER - currentState|`, which on
    /// our two legs is the open progress times what is left of the app-grid one. 0
    /// draws no icon, which is also the reference's "the transition never touches
    /// WINDOW_PICKER" case.
    pub fn preview_icon_scale(&self) -> f64 {
        self.expose_progress().unwrap_or(0.) * (1. - self.app_grid_leg())
    }

    /// Every drawn window preview and its hover alpha — the shared source of
    /// [`preview_overlays`](Self::preview_overlays) (hover-gated chrome) and of the
    /// app icon, which is *not* hover-gated and survives into the app-grid
    /// transition while its scale ramps out.
    pub fn preview_rects(&self) -> Vec<(W::Id, Rectangle<f64, Logical>, f64)> {
        let Some(progress) = self.expose_progress() else {
            return Vec::new();
        };

        let zoom = self.overview_zoom();
        let mut overlays = Vec::new();
        for ((idx, ws), geo) in self.workspaces_with_render_geo_idx() {
            let ws_zoom = zoom * self.workspace_render_scale(idx);
            // Every window, not just the hovered ones: the app icon is drawn for
            // all of them, and a hover of 0 is exactly what an un-hovered
            // preview's chrome should fade to.
            for window in ws.windows().map(|w| w.id().clone()).collect::<Vec<_>>() {
                let Some(rect) = ws.expose_drawn_rect(&window, progress, ws_zoom) else {
                    continue;
                };
                overlays.push((
                    window.clone(),
                    Rectangle::new(
                        geo.loc + rect.loc.upscale(ws_zoom),
                        rect.size.upscale(ws_zoom),
                    ),
                    ws.expose_hover_value(&window) * progress,
                ));
            }
        }
        overlays
    }

    pub fn window_under(&self, pos_within_output: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // With the app grid showing, the workspaces shrink into a row that is scenery, not a
        // picker: gnome-shell's workspace mode is 0 in the APP_GRID state
        // (`workspacesView.js:236`), and a preview's overlay — the hover growth, the close button,
        // the title — is enabled only at mode 1 (`workspace.js:775-777` `_syncOverlay`), with the
        // keyboard focus chain empty there too (`workspace.js:889-891`). The comparison is against
        // exactly 1, so the row goes inert the moment the transition starts — and comes back
        // live as the leg unwinds on the way out, which is the same comparison read backwards.
        if self.app_grid_leg() > 0. {
            return None;
        }

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

        // A scrolled row runs past the band it was allocated, and past that band is
        // where the floating search entry sits — so everything the strip draws is
        // clipped to it. The band slides with the row, or the clip would eat the whole
        // strip on the way in.
        let band = Rectangle::new(strip.band.loc + slide, strip.band.size);
        let band_physical = band.to_physical_precise_round(scale);
        // The clip is a *horizontal* concern: it exists to keep the row out of the
        // floating entry's column. A shadow clipped to the band as well is cut flat along
        // the thumbnail's top and bottom edges, which turns the active workspace's accent
        // glow into two side stripes — so shadows keep the band's x range and get the
        // band's height plus their own reach. It slides with the band, so the strip's
        // entrance still clips correctly.
        let glow_margin = SHADOW_GLOW_MARGIN * overview_layout::chrome_ramp(self.view_size);
        let glow_bounds_logical = Rectangle::new(
            band.loc - Point::from((0., glow_margin)),
            band.size + Size::from((0., glow_margin * 2.)),
        );
        // Each ring is in view coordinates already, so it clips against the band directly.
        let mut push_ring = |elem| {
            if let Some(elem) = CropRenderElement::from_element(elem, scale, band_physical) {
                let elem = MonitorInnerRenderElement::InsertHint(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::default(), 1.);
                let elem =
                    RelocateRenderElement::from_element(elem, Point::default(), Relocate::Relative);
                push(elem);
            }
        };

        // The new-workspace drop placeholder, while a drag hovers a gap. First pushed =
        // topmost, and it belongs over the row it is parting.
        if let Some(rect) = strip.placeholder {
            self.thumb_placeholder
                .render(rect.loc + slide, &mut push_ring);
        }

        for (idx, (ws, slot)) in zip(&self.workspaces, &strip.thumbs).enumerate() {
            let shrink = self.workspace_render_scale(idx);
            let thumb = thumbnail_drawn_rect(*slot, shrink, slide);
            let thumb_scale = strip.scale * shrink;
            let thumb_loc_physical = thumb.loc.to_physical_precise_round(scale);
            let xray_pos = XrayPos::new(thumb.loc, thumb_scale);

            // Clip each miniature to its workspace, and to the part of it the band
            // leaves visible. Both live in *workspace* coordinates, because the crop
            // is applied before the thumbnail's rescale and relocate.
            let x0 = ((band.loc.x - thumb.loc.x) / thumb_scale).max(0.);
            let x1 = ((band.loc.x + band.size.w - thumb.loc.x) / thumb_scale).min(self.view_size.w);
            if x1 <= x0 {
                // Scrolled entirely out of the band.
                continue;
            }
            let crop_bounds = Rectangle::new(
                Point::from((x0, 0.)),
                Size::from((x1 - x0, self.view_size.h)),
            )
            .to_physical_precise_round(scale);

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
                                thumb_scale,
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

            // The **picker's** layout, not the workspace's own: the row shows the same
            // spread previews the window picker does, at the row's scale, so the two read
            // as one thing at two sizes (and so the show-apps transition, which fades the
            // picker away over this row, has nothing to reconcile).
            ws.render_expose(ctx.r(), xray_pos, progress, thumb_scale, push_thumb!());

            // The wallpaper behind, rounded exactly as the shadow under it is — the
            // drawn radius expressed in workspace coordinates, since this is applied
            // before the rescale. The solid color backs workspaces without one.
            let mut wallpapered = false;
            if let Some(wallpaper) = wallpaper {
                // The picker's own background radius, in workspace coordinates — this
                // miniature is the picker scaled, corners included, and the shadow under
                // it is baked on the very same radius (a square-cornered shadow around a
                // rounded background shows through each corner as a pointy tab).
                let radius = self.workspace_background_radius();
                if let Some(elem) = wallpaper.render(
                    ctx.renderer,
                    Default::default(),
                    ws.view_size(),
                    radius,
                    Scale::from(scale * thumb_scale),
                ) {
                    if let Some(elem) = CropRenderElement::from_element(elem, scale, crop_bounds) {
                        let elem = MonitorInnerRenderElement::CroppedRoundedTexture(elem);
                        let elem = RescaleRenderElement::from_element(
                            elem,
                            Point::from((0, 0)),
                            thumb_scale,
                        );
                        let elem = RelocateRenderElement::from_element(
                            elem,
                            thumb_loc_physical,
                            Relocate::Relative,
                        );
                        push(elem);
                    }
                    wallpapered = true;
                }
            }
            if !wallpapered {
                if let Some(elem) =
                    CropRenderElement::from_element(ws.render_background(), scale, crop_bounds)
                {
                    let elem = MonitorInnerRenderElement::CroppedSolidColor(elem);
                    let elem =
                        RescaleRenderElement::from_element(elem, Point::from((0, 0)), thumb_scale);
                    let elem = RelocateRenderElement::from_element(
                        elem,
                        thumb_loc_physical,
                        Relocate::Relative,
                    );
                    push(elem);
                }
            }

            // Last pushed = bottommost: the shadow under everything the thumbnail draws —
            // the very shadow the workspace casts in the window picker, drawn through this
            // miniature's own transform, so the two rows are shadowed alike and the shadow
            // cannot drift from its caster mid-animation. The active workspace gets the
            // accent one, our replacement for gnome-shell's indicator ring.
            //
            // The crop is expressed in the same pre-transform (workspace) space as the
            // contents' one above, widened by the glow's reach so the accent halo is not
            // sliced flat along the band's top and bottom edges.
            let glow_crop = Rectangle::new(
                Point::from((
                    (glow_bounds_logical.loc.x - thumb.loc.x) / thumb_scale,
                    (glow_bounds_logical.loc.y - thumb.loc.y) / thumb_scale,
                )),
                glow_bounds_logical.size.downscale(thumb_scale),
            )
            .to_physical_precise_round(scale);
            let push_shadow = &mut |elem: ShadowRenderElement| {
                let elem = elem.with_alpha(progress.clamp(0., 1.) as f32);
                if let Some(elem) = CropRenderElement::from_element(elem, scale, glow_crop) {
                    let elem = MonitorInnerRenderElement::CroppedShadow(elem);
                    let elem =
                        RescaleRenderElement::from_element(elem, Point::from((0, 0)), thumb_scale);
                    let elem = RelocateRenderElement::from_element(
                        elem,
                        thumb_loc_physical,
                        Relocate::Relative,
                    );
                    push(elem);
                }
            };
            if idx == self.active_workspace_idx {
                self.active_shadow.render(Point::default(), push_shadow);
            } else {
                ws.render_shadow(push_shadow);
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
        //
        // The window picker is the exception — see `expose_crop_bounds` below.
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
            uncropped()
        };

        // The window picker does not clip its previews to the workspace: gnome-shell's
        // `WorkspaceLayout.vfunc_allocate` calls `container.remove_clip()` for every state up to
        // and including WINDOW_PICKER (`js/ui/workspace.js:653-665`), and only the *background*
        // clips to its allocation (`WorkspaceBackground._bin`, `clip_to_allocation: true`,
        // `js/ui/workspace.js:967`). Previews are meant to spill past the workspace's rounded
        // rect — a preview against the edge otherwise loses its shadow and looks cramped.
        let expose_crop_bounds = uncropped();

        let zoom = self.overview_zoom();

        let insert_hint_render_loc = self
            .insert_hint_render_loc
            .filter(|_| !self.options.layout.insert_hint.off);

        // The workspace the row sits on draws a touch larger than its neighbors
        // (`workspace_render_scale`), so the zoom is per workspace, not per
        // monitor.
        let scale_relocate = move |ws_zoom: f64, geo: Rectangle<f64, Logical>, elem| {
            let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), ws_zoom);
            RelocateRenderElement::from_element(
                elem,
                // The offset we get from workspaces_with_render_geo() is already
                // rounded to physical pixels, but it's in the logical coordinate
                // space, so we need to convert it to physical.
                geo.loc.to_physical_precise_round(scale),
                Relocate::Relative,
            )
        };

        for ((idx, ws), geo) in self.workspaces_with_render_geo_idx() {
            let ws_zoom = zoom * self.workspace_render_scale(idx);
            // Macro instead of closure because ws and insert hint have different elem types.
            macro_rules! push_cropped_to {
                ($bounds:expr) => {{
                    &mut |elem| {
                        let elem = CropRenderElement::from_element(elem, scale, $bounds);
                        if let Some(elem) = elem {
                            let elem = MonitorInnerRenderElement::from(elem);
                            push(scale_relocate(ws_zoom, geo, elem));
                        }
                    }
                }};
            }
            macro_rules! push {
                () => {
                    push_cropped_to!(crop_bounds)
                };
            }

            let xray_pos = XrayPos::new(geo.loc, ws_zoom);

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
                ws.render_expose(
                    ctx.r(),
                    xray_pos,
                    progress,
                    ws_zoom,
                    push_cropped_to!(expose_crop_bounds),
                );
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

        for ((idx, ws), geo) in self.workspaces_with_render_geo_idx() {
            let ws_zoom = zoom * self.workspace_render_scale(idx);
            let push_shadow: &mut dyn FnMut(ShadowRenderElement) = &mut |elem| {
                let elem = elem.with_alpha(alpha);
                let elem = MonitorInnerRenderElement::Shadow(elem);
                let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), ws_zoom);
                let elem = RelocateRenderElement::from_element(
                    elem,
                    geo.loc.to_physical_precise_round(scale),
                    Relocate::Relative,
                );
                push(elem);
            };
            // Deliberately *not* the accent shadow the small row puts under its active
            // workspace: the row is a switcher, where "this is the one you are on" is
            // worth a cue, while the picker only ever shows one workspace whole and
            // centered. A halo there is decoration on something already unambiguous.
            ws.render_shadow(push_shadow);
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

    pub fn layout_config(&self) -> Option<&synoik_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub(super) fn verify_invariants(&self) {
        use approx::assert_abs_diff_eq;

        let options =
            Options::clone(&self.base_options).with_merged_layout(self.layout_config.as_ref());
        assert_eq!(&*self.options, &options);

        assert!(
            self.workspaces.len() >= MIN_NUM_WORKSPACES,
            "monitor must have at least {MIN_NUM_WORKSPACES} workspaces"
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

        assert!(
            self.workspaces.last().unwrap().name.is_none(),
            "monitor must have an unnamed workspace in the end"
        );
        // NOTE: empty non-last non-active workspaces are legal here — they are the whole
        // point of the manual-close divergence (see `clean_up_workspaces`). What is *not*
        // legal is a monitor with no trailing empty, which the two asserts above pin.

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

/// How much the workspace at `idx` is scaled about its own center, given where
/// the row sits (`scroll_position`) and how far the shrink is faded in (`ramp`).
///
/// `WorkspacesView._updateWorkspacesState` (`workspacesView.js:243-266`) keeps
/// every workspace at [`WORKSPACE_INACTIVE_SCALE`] and grows it back to 1 as the
/// row scrolls onto it — `lerp(0.94, 1, 1 - clamp(|value - i|, 0, 1))` — about a
/// centered pivot (`workspace.js:1039`). It is what makes the workspace you are
/// on read as slightly larger than its neighbors, in the window picker and in
/// the app grid's fitted row alike.
///
/// **Divergence.** gnome-shell applies this to actors that only exist inside the
/// overview, so it can leave them scaled unconditionally. Our row *is* the
/// desktop, so the shrink is ramped in with the overview progress. At rest the
/// two agree — the workspace you are on is 1 either way; what the ramp avoids is
/// a plain desktop workspace switch, where the scroll position is briefly
/// fractional, shrinking both workspaces on screen to 0.97.
///
/// A free function so the render-geometry iterator can call it from a `move`
/// closure over plain `Copy` inputs without re-deriving the formula.
fn workspace_render_scale(scroll_position: f64, idx: usize, ramp: f64) -> f64 {
    let distance = (scroll_position - idx as f64).abs().clamp(0., 1.);
    1. - (1. - WORKSPACE_INACTIVE_SCALE) * distance * ramp
}

/// Where a thumbnail is actually *drawn* inside its strip slot: inactive workspaces
/// shrink about the slot's centre, exactly as they do in the row this strip is modelled
/// on (`_updateWorkspacesState`, `workspacesView.js:243-266`) — the strip's own "which one
/// am I on" cue, under the accent glow.
///
/// One expression, because the close button has to land on the drawn rect and not on the
/// slot: the workspaces that grow a close button are the empty ones, which are hardly ever
/// the active one, so they are hardly ever drawn at slot size.
fn thumbnail_drawn_rect(
    slot: Rectangle<f64, Logical>,
    shrink: f64,
    slide: Point<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let size = slot.size.downscale(1. / shrink);
    Rectangle::new(
        slot.loc + slide + Point::from(((slot.size.w - size.w) / 2., (slot.size.h - size.h) / 2.)),
        size,
    )
}

/// The fit-single row: the active workspace centered in the view, its neighbours
/// hanging off the edges (`_getFirstFitSingleWorkspaceBox`,
/// `workspacesView.js:171-204`). Returns the first workspace's absolute
/// strip-axis position and the row's pitch.
///
/// Pure algebra, so the row's shape is pinned without a clock (the animated
/// geometry that goes through it is sampled separately).
fn fit_single_row(view_w: f64, ws_w: f64, gap: f64, render_idx: f64) -> (f64, f64) {
    let extent = ws_w + gap;
    ((view_w - ws_w) / 2. - render_idx * extent, extent)
}

/// Where a row of `run` length sits inside a `span`-wide viewport so that the item at
/// `focus` (a distance from the row's start) stays visible: a row that fits is centered,
/// and one that does not scrolls to center the focused item, clamped so the row never
/// leaves a gap at either end.
///
/// This is the one rule both overview rows follow when they overflow — the workspace row
/// ([`fit_all_row`]) and the thumbnail strip
/// ([`crate::layout::thumbnails::strip_geometry`]).
pub(super) fn scroll_to_follow(span: f64, run: f64, focus: f64) -> f64 {
    if run <= span {
        // gnome-shell's centering: `Math.max((availableWidth - workspaceWidth * n) / 2, 0)`.
        // There is also nothing to clamp against here — `span - run` is the *upper* bound,
        // so the clamp below would be inverted.
        return (span - run) / 2.;
    }
    center_on_focus(span, focus).clamp(span - run, 0.)
}

/// The offset that puts `focus` (a distance from the row's start) on the viewport's center,
/// whatever the row's length: the focused item sticks to the middle, and the ends carry dead
/// space rather than being pulled flush.
///
/// This is what the thumbnail strip does, and it is the picker's own fit-single behaviour
/// ([`fit_single_row`]). It buys the strip its "there is more this way" affordance for free:
/// with the active thumbnail pinned to the middle, an overflowing side almost always has a
/// workspace *poking in* at the band edge rather than ending on a whole one, and dead space
/// at an end reads, correctly, as "nothing further this way".
///
/// **It applies even when the whole row fits.** Centering a short row *as a whole* instead —
/// which is what gnome-shell does, and what [`scroll_to_follow`] keeps — leaves the active
/// workspace off center by up to half the row, which is very visible: four workspaces on a
/// 3072-wide canvas fit their band comfortably and put the active one 714px left of the
/// middle. The cost is that a two- or three-workspace strip now slides on every switch.
///
/// The app-grid row uses [`scroll_to_follow`] instead: it spans the whole screen, so an
/// unclamped end would leave most of a screen width empty beside the last workspace.
pub(super) fn center_on_focus(span: f64, focus: f64) -> f64 {
    span / 2. - focus
}

/// The strip's drop placeholder: a translucent pill marking where the new
/// workspace goes (gnome-shell's workspace-placeholder asset).
fn thumbnail_placeholder_config() -> synoik_config::FocusRing {
    let color = synoik_config::Color::from_rgba8_unpremul(0xff, 0xff, 0xff, 0x66);
    synoik_config::FocusRing {
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

/// Clock-free sweeps over the two row layouts. The animated geometry that lerps
/// between them is pinned separately, by sampling the real transition
/// (`overview_grid_transition_moves_the_row_monotonically` in the conformance
/// corpus); here we pin the endpoints themselves, as plain algebra, so a
/// regression in either row is attributable without an animation in the picture.
#[cfg(test)]
mod row_tests {
    use super::fit_single_row;

    /// The view, a 16:9 workspace zoomed to fit, and a gap — one plausible set of
    /// inputs to sweep the interesting parameters around.
    const VIEW: f64 = 1920.;

    fn single_positions(ws_w: f64, gap: f64, render_idx: f64, n: usize) -> Vec<f64> {
        let (x1, pitch) = fit_single_row(VIEW, ws_w, gap, render_idx);
        (0..n).map(|i| x1 + i as f64 * pitch).collect()
    }

    /// The whole point of fit-single: whichever workspace is active is the one
    /// centered in the view, at every integer index and for every row length.
    #[test]
    fn fit_single_centers_the_active_workspace() {
        for ws_w in [1920., 1400., 960.] {
            for gap in [0., 32., 100.] {
                for n in 1..8usize {
                    for active in 0..n {
                        // The row scrolled to `active` must put *that* workspace's
                        // center on the view's center.
                        let xs = single_positions(ws_w, gap, active as f64, n);
                        let center = xs[active] + ws_w / 2.;
                        assert!(
                            (center - VIEW / 2.).abs() < 1e-9,
                            "ws {active} of {n} off center at {center} \
                             (ws_w={ws_w}, gap={gap})"
                        );
                    }
                }
            }
        }
    }

    /// A fractional `render_idx` — mid workspace-switch — slides the row by that
    /// fraction of the pitch and nothing else. The pitch is index-independent, so
    /// the row is rigid: it translates, it never stretches.
    #[test]
    fn fit_single_is_a_rigid_translation() {
        let (ws_w, gap) = (1400., 32.);
        let (base, pitch) = fit_single_row(VIEW, ws_w, gap, 0.);
        for step in 0..=20 {
            let idx = f64::from(step) / 10.;
            let (x1, p) = fit_single_row(VIEW, ws_w, gap, idx);
            assert!((p - pitch).abs() < 1e-9, "pitch moved at render_idx={idx}");
            assert!(
                (x1 - (base - idx * pitch)).abs() < 1e-9,
                "row not rigid at render_idx={idx}"
            );
        }
    }
}
