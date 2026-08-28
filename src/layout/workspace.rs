// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::{Cell, RefCell};
use std::cmp::max;
use std::iter;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::desktop::{layer_map_for_output, Window};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;
use synoik_config::utils::MergeWith as _;
use synoik_config::{
    CenterFocusedColumn, CornerRadius, PresetSize, WindowingMode, Workspace as WorkspaceConfig,
};
use synoik_ipc::{ColumnDisplay, PositionChange, SizeChange, WindowLayout};

use super::floating::{FloatingSpace, FloatingSpaceRenderElement};
use super::scrolling::{
    Column, ColumnWidth, ScrollDirection, ScrollingSpace, ScrollingSpaceRenderElement,
};
use super::shadow::Shadow;
use super::tile::{SnapshotRenderer, Tile, TileRenderElement, TileUnmapSnapshot};
use super::{
    expose, ActivateWindow, HitType, InsertPosition, InteractiveResizeData, LayoutElement, Options,
    RemovedTile, SizeFrac, TileSessionState,
};
use crate::animation::{Animation, Clock, Curve};
use crate::gnome::{EdgeTileTarget, TileSide};
use crate::output_identity::OutputIdentity;
use crate::render_helpers::shadow::ShadowRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::RenderCtx;
use crate::synoik_render_elements;
use crate::utils::id::IdCounter;
use crate::utils::transaction::{Transaction, TransactionBlocker};
use crate::utils::{
    center_preferring_top_left_in_area, ensure_min_max_size, ensure_min_max_size_maybe_zero,
    output_size, send_scale_transform, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

/// Where a window this workspace holds actually is.
///
/// Three-state because it must be. The `if self.floating.has_window(id) { … } else { … }` this
/// replaces read the scrolling half as "everything that is not floating", which stopped being
/// true the moment a window could be minimized — and the scrolling half's lookups `unwrap`, so a
/// wrong guess is a panic, not a no-op. Every dispatch matches on this **exhaustively, with no
/// catch-all**, so adding or removing a mode makes the compiler name every site that has to
/// answer for it rather than letting one hide in an `else`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Home {
    /// Laid out by the scrolling half.
    Scrolling,
    /// Laid out by the floating half.
    Floating,
    /// Parked minimized: on the workspace, in neither half, with no geometry. Every operation
    /// that asks "where is this window" has nothing to do for one of these.
    Minimized,
}

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// The scrollable-tiling layout.
    scrolling: ScrollingSpace<W>,

    /// The floating layout.
    floating: FloatingSpace<W>,

    /// Whether the floating layout is active instead of the scrolling layout.
    floating_is_active: FloatingActive,

    /// Windows on this workspace that are minimized: alive, still ours, and out of both layouts.
    ///
    /// mutter keeps a plain `window->minimized` bool and lets `CALC_SHOWING` hide the actor
    /// (`window.c:2734-2771`), because its windows float freely and hiding one changes no other
    /// window's geometry. Ours can be tiled, where "not showing" and "still in the column" cannot
    /// both be true — so a minimized window is *taken out* of the layout instead, exactly the way
    /// an interactive move takes one out (`InteractiveMoveState::Moving`), and put back on
    /// unminimize. [`RemovedTile`] is what it was taken out with, so putting it back is exact.
    ///
    /// It is parked on the **workspace**, not on `Layout`: everything that reads windows through
    /// `Layout::workspaces()` — the session store above all (`Layout::session_snapshot`) — would
    /// otherwise stop seeing it, and a minimized window would silently stop being savable.
    /// [`Self::tiles`] chains it, which is what keeps the switcher, IPC, the app system and the
    /// foreign-toplevel sweep seeing it; the render and hit-test paths go through
    /// [`Self::tiles_with_render_positions`] instead, which does not, so nothing draws it and
    /// nothing can click it.
    minimized: Vec<Parked<W>>,

    /// The original output of this workspace.
    ///
    /// Most of the time this will be the workspace's current output, however, after an output
    /// disconnection, it may remain pointing to the disconnected output.
    pub(super) original_output: OutputIdentity,

    /// Where in its home display's strip this workspace sat.
    ///
    /// Relative order is all a reclaim could restore without this: a workspace reordered while its
    /// display was away came back wherever the evacuation had left it. Best-effort by construction
    /// — a homeless workspace's ordinal is an index into the display it is *visiting*, which is
    /// the price of not snapshotting the whole arrangement (`docs/fork/multi-display.md` §2).
    pub(super) home_ordinal: usize,
    /// Whether session restore has spoken for this workspace ([`Self::claim_for`]).
    pub(super) claimed: bool,

    /// Current output of this workspace.
    output: Option<Output>,

    /// Latest known output scale for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    scale: smithay::output::Scale,

    /// Latest known output transform for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    transform: Transform,

    /// Latest known view size for this workspace.
    ///
    /// This should be computed from the current workspace output size, or, if all outputs have
    /// been disconnected, preserved until a new output is connected.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this workspace.
    ///
    /// Not rounded to physical pixels.
    ///
    /// This is similar to view size, but takes into account things like layer shell exclusive
    /// zones.
    working_area: Rectangle<f64, Logical>,

    /// This workspace's shadow in the overview.
    shadow: Shadow,

    /// This workspace's background.
    background_buffer: SolidColorBuffer,

    /// Clock for driving animations.
    pub(super) clock: Clock,

    /// Configurable properties of the layout as received from the parent monitor.
    pub(super) base_options: Rc<Options>,

    /// Configurable properties of the layout with logical sizes adjusted for the current `scale`.
    pub(super) options: Rc<Options>,

    /// Optional name of this workspace.
    pub(super) name: Option<String>,

    /// Layout config overrides for this workspace.
    layout_config: Option<synoik_config::LayoutPart>,

    /// The window being dragged out of this workspace's picker, kept as a layout input for
    /// as long as the drag lasts — see [`Workspace::freeze_expose`].
    expose_reserved: Option<ExposeInput>,

    /// The picker's layout, held past a removal — see [`CloseFreeze`].
    expose_freeze: Option<CloseFreeze>,

    /// The picker's held layout — see [`RetainedExpose`]. `RefCell` because
    /// [`Workspace::expose_layout`] is a read: the layout is a value the workspace
    /// remembers, not a thing a caller has to ask it to update.
    expose_retained: RefCell<Option<RetainedExpose>>,

    /// How many times the picker has decided a layout. The claim retention makes is about
    /// *when work happens*, and this is the only way to observe it: a held layout and a
    /// freshly derived one are otherwise indistinguishable by construction.
    expose_recomputes: Cell<u64>,

    /// Previews easing from the slots they held to the ones the picker now gives them —
    /// see [`Workspace::slide_expose_slots_from`].
    expose_slides: ExposeSlides<W>,

    /// Per-window picker-overlay progress: 0 idle, 1 fully hovered. gnome-shell's
    /// `showOverlay`/`hideOverlay` ease the pointed-at preview up to
    /// `WINDOW_ACTIVE_SIZE_INC` bigger and back down again
    /// (`windowPreview.js:310-390`). At most one entry is rising; entries that
    /// have fallen back to 0 are dropped.
    expose_hover: ExposeHovers<W>,

    /// Unique ID of this workspace.
    id: WorkspaceId,
}

static WORKSPACE_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    fn next() -> WorkspaceId {
        WorkspaceId(WORKSPACE_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}

/// gnome-shell's `MINIMIZE_WINDOW_ANIMATION_TIME` (`windowManager.js:28`), whose
/// `MINIMIZE_WINDOW_ANIMATION_MODE` is `EASE_OUT_EXPO` (`:29`). Minimize and unminimize are one
/// mechanism there — the same constant, the same mode, mirrored — so they are here too.
const MINIMIZE_ANIMATION_MS: u64 = 400;

/// [`MINIMIZE_ANIMATION_MS`] as the config animation the tile-level helpers take, so the fade and
/// the grow land on the same clock as the geometry they accompany. An animation whose halves end
/// at different times reads as two animations.
fn minimize_animation_config() -> synoik_config::Animation {
    synoik_config::Animation {
        off: false,
        kind: synoik_config::animations::Kind::Easing(synoik_config::animations::EasingParams {
            duration_ms: MINIMIZE_ANIMATION_MS as u32,
            curve: synoik_config::animations::Curve::EaseOutExpo,
        }),
    }
}

/// The smallest a destination rect may be. The picker interpolates a *scale* out of it
/// (`slot.size.w / rect.size.w`), so a zero-sized destination divides by zero and the preview
/// never draws — the defect fixed in `b8078c6f`. gnome-shell ends its own minimize at scale 0
/// because the window is invisible by then; ours fades too, but the destination stays a real rect
/// so that every scale derived from it is finite regardless of where the fade has got to.
const MIN_DEST_SIZE: f64 = 8.;

/// A window parked by a minimize, and where it was seen to go.
#[derive(Debug)]
struct Parked<W: LayoutElement> {
    /// The exact take-out record, so putting it back is a put-back and not a re-place.
    removed: RemovedTile<W>,
    /// The rect the window shrank into, in workspace coordinates. The picker grows the preview
    /// back out of this same rect, so the desktop motion and the overview motion agree about
    /// where the window went.
    ///
    /// Always known, even when nothing was animated into it: where a hidden window *lives* is
    /// not the same question as whether the user watched it go there. A session restore parks a
    /// window that was never on screen and a minimize with the overview up has no desktop to
    /// cross — neither shrinks, but both must still grow out of the dock when the picker opens,
    /// or the preview appears at full size somewhere the user never saw it.
    dest: Rectangle<f64, Logical>,
    /// The shrink itself, while it runs. `None` once it has landed.
    shrink: Option<Shrink>,
}

/// The 400ms shrink from a window's last on-screen rect into [`Parked::dest`].
#[derive(Debug)]
struct Shrink {
    from: Rectangle<f64, Logical>,
    anim: Animation,
}

/// Clamp a destination to [`MIN_DEST_SIZE`].
fn sane_dest(dest: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        dest.loc,
        Size::from((
            dest.size.w.max(MIN_DEST_SIZE),
            dest.size.h.max(MIN_DEST_SIZE),
        )),
    )
}

impl<W: LayoutElement> Parked<W> {
    /// Where the tile draws right now, while the shrink runs.
    fn shrinking_rect(&self) -> Option<Rectangle<f64, Logical>> {
        let shrink = self.shrink.as_ref()?;
        let dest = self.dest;
        let t = shrink.anim.clamped_value();
        let lerp = |a: f64, b: f64| a + (b - a) * t;
        Some(Rectangle::new(
            Point::from((
                lerp(shrink.from.loc.x, dest.loc.x),
                lerp(shrink.from.loc.y, dest.loc.y),
            )),
            Size::from((
                lerp(shrink.from.size.w, dest.size.w),
                lerp(shrink.from.size.h, dest.size.h),
            )),
        ))
    }
}

/// Picker layout: (tile, current rect, slot rect, scale at progress 0), workspace coordinates.
///
/// **`rect.size` is the tile's natural size, always.** The draw scale is derived from it
/// (`slot.size.w / rect.size.w`) and then applied to the tile's own natural-size elements, so a
/// `rect` sized as anything else scales the window by exactly that ratio — a dock-icon-sized one
/// drew a window forty times too big, one corner of it covering the workspace.
///
/// A preview that starts somewhere it was never drawn says so with the fourth field instead: the
/// scale it has at progress 0, which is 1 for a tile that is simply where it looks like it is.
type ExposeLayout<'a, W> = Vec<(
    &'a Tile<W>,
    Rectangle<f64, Logical>,
    Rectangle<f64, Logical>,
    f64,
)>;

/// One input to the picker's layout: a window's stable sequence, and the rect it is laid
/// out over.
///
/// The sequence rather than the id: identity is all the comparison needs, and an id here is
/// a `smithay::desktop::Window` handle that the held layout would keep alive for as long as
/// it holds it — which is until the next picker query on that workspace, i.e. the next
/// overview visit. A window closing behind a shut overview must not wait on that.
type ExposeInput = (u64, Rectangle<f64, Logical>);

/// The picker's standing layout decision, and the exact inputs it was reached from.
///
/// Held rather than re-derived because deciding is the half that *orders*: the row and
/// column sorts in [`expose::compute_grid`] are stable with no tie-break, and centred
/// placement makes exact ties ordinary, so re-running them over inputs that moved by a
/// sub-pixel re-seats previews that had no business moving.
#[derive(Debug)]
struct RetainedExpose {
    /// Every input the decision was reached from, in the order the grid's rows index —
    /// stable creation order. Validity is bit-equality against this, recomputed per call.
    ///
    /// **Comparing the inputs rather than dirtying on the events that change them** is a
    /// deliberate departure from what a dirty flag would do. Under-invalidation is this
    /// design's one real hazard — a permanently wrong picker is worse than a transient
    /// wobble — and the mutation surface cannot be closed by inspection:
    /// `tiles_with_offsets_mut` (`floating.rs`) hands out `&mut Tile` past every named
    /// mutator, and an interactive resize reaches a window's size through it. Comparison
    /// makes a missed event unrepresentable: the worst it can do is recompute.
    inputs: Vec<ExposeInput>,
    /// The view the grid was decided for. Compared like the inputs, because the monitor is
    /// frozen into the decision — `window_scale`'s enlargement of small windows is read
    /// again at packing time from the height the grid was summed at, so a held grid packed
    /// against a taller view would scale previews by a rule the view contradicts.
    /// gnome-shell freezes the same thing, constructing its layout strategy around
    /// `Main.layoutManager.monitors[this._monitorIndex]` on every decision
    /// (`workspace.js:521-522`, read back in `_computeWindowScale` at `:173`).
    ///
    /// Both dimensions, not just the height the grid needs: a mode change from 1920x1080 to
    /// 3440x1080 leaves the height bit-identical while the area doubles in width, and a row
    /// count searched for half the width has no business surviving it.
    ///
    /// The **area** is deliberately absent. Packing the held decision into a changed area
    /// rather than re-deciding is the entire point of holding one, and it is gnome-shell's:
    /// `_layout` is guarded by `_needsLayout` while `_windowSlots` recomputes on
    /// `containerAllocationChanged` (`workspace.js:668-681`), and a `workareas-changed`
    /// calls `layout_changed()` without ever setting `_needsLayout` (`:594-597`).
    view_size: Size<f64, Logical>,
    grid: expose::GridLayout,
}

/// Bit-equality of two layout inputs.
///
/// Bits, not `==`: `-0.` and `0.` compare equal but sort apart under `total_cmp`, which is
/// what the grid orders with, so a value that flipped sign of zero really can re-seat a
/// preview. `NaN` going the other way — never equal to itself — only costs a recompute.
fn same_expose_input(a: &ExposeInput, b: &ExposeInput) -> bool {
    let bits = |r: &Rectangle<f64, Logical>| {
        [
            r.loc.x.to_bits(),
            r.loc.y.to_bits(),
            r.size.w.to_bits(),
            r.size.h.to_bits(),
        ]
    };
    a.0 == b.0 && bits(&a.1) == bits(&b.1)
}

/// Picker-overlay progress per window — see [`Workspace::expose_hover`].
type ExposeHovers<W> = Vec<(<W as LayoutElement>::Id, Animation)>;

/// One preview crossing from the slot it held to the one it has now — see
/// [`Workspace::slide_expose_slots_from`].
#[derive(Debug)]
struct SlotSlide {
    /// The slot it is coming from, in workspace coordinates.
    from: Rectangle<f64, Logical>,
    /// 0 = at `from`, 1 = at the slot the picker computes today.
    anim: Animation,
}

type ExposeSlides<W> = Vec<(<W as LayoutElement>::Id, SlotSlide)>;

/// The picker's layout, held past a removal so the previews do not reflow out from under a
/// pointer that is still working in them — gnome-shell's `layout_frozen`
/// (`_doRemoveWindow`, `workspace.js:1140-1183`).
///
/// The hole the departed window leaves stays open for as long as this lives, and closes with
/// the ordinary 200ms ease when it is released. That is gnome-shell's ordering, not an
/// embellishment: clearing `layout_frozen` emits `layout_changed()` (`:937`) onto a
/// `_needsLayout` that `removeWindow` set at `:857`, so the next allocate recomputes with
/// `layoutChanged` true and every child goes through `animateAllocation` (`:759-766`). **The
/// ease belongs to the release, not to the close.**
#[derive(Debug)]
struct CloseFreeze {
    /// The whole input list as of just before the removal, drag reservation included.
    ///
    /// The whole list rather than the departed window's entry alone, which is what freezing
    /// looks like from [`Workspace::expose_layout`]: a removal in the scrolling layout shifts
    /// every column after it, so the *survivors'* settled rects move too and holding one
    /// vacant entry would re-decide the grid over them anyway — and snap them, since the
    /// ease is not armed until the release. gnome-shell freezes the allocation itself and has
    /// no such hole.
    ///
    /// Retention is not bypassed by this, only fed: [`Workspace::retained_expose_slots`] still
    /// validates the held decision bit-for-bit against exactly what it is handed, which while
    /// frozen is this list, so it hits.
    inputs: Vec<ExposeInput>,
    /// `None` holds indefinitely — the pointer is resting on one of this workspace's previews,
    /// where gnome-shell's tick keeps returning `SOURCE_CONTINUE` (`workspace.js:1170`), and
    /// where a close button has to stay put for a second click. Deliberately claims no
    /// animation: the hold lasts as long as the user leaves the pointer there, and pinning the
    /// frame loop for that would cost 60fps of an unchanging picture.
    ///
    /// `Some` releases when it is done, and *does* claim one — which is what guarantees a
    /// frame arrives to notice, at a bounded cost of [`CLOSE_FREEZE_MS`] of static redraws.
    /// gnome-shell pays nothing there, its GLib timeout driving no damage; this is the price
    /// of expiring on the animation clock instead, which is what makes the hold testable
    /// under a frozen clock at all.
    hold: Option<Animation>,
}

/// `WINDOW_REPOSITIONING_DELAY` (`workspace.js:20`): how long the pointer must hold still
/// before a removal is allowed to reflow the picker.
const CLOSE_FREEZE_MS: u32 = 750;

/// gnome-shell eases a preview from its current allocation to a new one over
/// `WINDOW_REPOSITIONING_DELAY`-free `Workspace._syncWindowPositions`: 200ms `EASE_OUT_QUAD`
/// (`workspace.js:759-766`, `animateAllocation` at `:389-399`).
const SLOT_SLIDE_MS: u32 = 200;

/// How much bigger a hovered preview gets, in each direction
/// (`WINDOW_ACTIVE_SIZE_INC`, `windowPreview.js:20`). GNOME multiplies it by the
/// theme scale factor because its stage is in device pixels; ours is logical, so
/// the render scale applies it for us.
const WINDOW_ACTIVE_SIZE_INC: f64 = 5.;

/// `WINDOW_SCALE_TIME` (`windowPreview.js:19`) — the fixed duration gnome-shell
/// eases the hover scale over, EASE_OUT_QUAD. Not the configurable overview
/// animation; only whether animations run at all is inherited.
const WINDOW_SCALE_TIME_MS: u32 = 200;

/// The hover ease itself — see [`WINDOW_SCALE_TIME_MS`].
fn ease_hover(clock: &Clock, from: f64, to: f64, options: &Options) -> Animation {
    let config = synoik_config::Animation {
        off: options.animations.overview_open_close.0.off,
        kind: synoik_config::animations::Kind::Easing(synoik_config::animations::EasingParams {
            duration_ms: WINDOW_SCALE_TIME_MS,
            curve: synoik_config::animations::Curve::EaseOutQuad,
        }),
    };
    Animation::new(clock.clone(), from, to, 0., config)
}

/// How much bigger a preview draws at hover progress `hover`: gnome-shell grows
/// the longest side by `WINDOW_ACTIVE_SIZE_INC` in each direction and scales the
/// whole container by that ratio (`windowPreview.js:340-352`).
///
/// `size` is the preview's size *on screen*: gnome-shell allocates its previews
/// in stage coordinates (the workspace scale is baked into the slots by
/// `WorkspaceLayout.vfunc_allocate`, `workspace.js:690-736`), so the 5px is 5
/// screen pixels however far the workspace row is zoomed out. We render a
/// workspace in its own coordinates and zoom the whole thing, so the caller
/// hands over the zoomed size and gets a scale to apply in workspace space.
fn hover_scale(size: Size<f64, Logical>, hover: f64) -> f64 {
    let longest = f64::max(size.w, size.h);
    if longest <= 0. {
        return 1.;
    }
    (longest + 2. * WINDOW_ACTIVE_SIZE_INC * hover) / longest
}

/// Where one preview draws and at what scale: its rect interpolated toward its
/// picker slot by `progress`, then grown about its center by the hover overlay.
/// The single source of the picker's drawn geometry — rendering and the
/// geometry accessors both go through it.
fn expose_tile_render(
    rect: Rectangle<f64, Logical>,
    slot: Rectangle<f64, Logical>,
    from_scale: f64,
    hover: f64,
    progress: f64,
    zoom: f64,
) -> (Point<f64, Logical>, f64) {
    let target_scale = slot.size.w / rect.size.w;
    let tile_scale = from_scale + (target_scale - from_scale) * progress;
    let pos = Point::from((
        rect.loc.x + (slot.loc.x - rect.loc.x) * progress,
        rect.loc.y + (slot.loc.y - rect.loc.y) * progress,
    ));

    // Hovering grows the preview about its center, so the slot it sits in doesn't
    // move (`showOverlay` scales `window_container`, whose growth
    // `_adjustOverlayOffsets` splits in half, `windowPreview.js:389-400`). The
    // growth also fades in with the overview: on the desktop there is no picker
    // and nothing to hover.
    let drawn = rect.size.upscale(tile_scale);
    let hover_scale = hover_scale(drawn.upscale(zoom), hover * progress);
    let pos = pos
        - Point::from((
            drawn.w * (hover_scale - 1.) / 2.,
            drawn.h * (hover_scale - 1.) / 2.,
        ));

    (pos, tile_scale * hover_scale)
}

/// Where one exposé preview goes: the position its elements draw at, and the factor
/// they are then rescaled by about that position — snapped to physical pixels, which
/// is what separates this from [`expose_tile_render`].
#[allow(clippy::too_many_arguments)]
fn expose_tile_placement(
    rect: Rectangle<f64, Logical>,
    slot: Rectangle<f64, Logical>,
    from_scale: f64,
    hover: f64,
    progress: f64,
    zoom: f64,
    scale: f64,
) -> (Point<f64, Logical>, f64) {
    let (pos, tile_scale) = expose_tile_render(rect, slot, from_scale, hover, progress, zoom);

    // Round to physical pixels.
    let pos = pos.to_physical_precise_round(scale).to_logical(scale);

    (pos, tile_scale)
}

synoik_render_elements! {
    WorkspaceRenderElement => {
        Scrolling = ScrollingSpaceRenderElement,
        Floating = FloatingSpaceRenderElement,
        Expose = RescaleRenderElement<TileRenderElement>,
    }
}

#[derive(Debug)]
pub(super) struct InteractiveResize<W: LayoutElement> {
    pub window: W::Id,
    pub original_window_size: Size<f64, Logical>,
    pub data: InteractiveResizeData,
}

/// Resolved width or height in logical pixels.
#[derive(Debug, Clone, Copy)]
pub enum ResolvedSize {
    /// Size of the tile including borders.
    Tile(f64),
    /// Size of the window excluding borders.
    Window(f64),
}

/// Whether the floating space is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingActive {
    /// The scrolling space is active.
    No,
    /// The scrolling space is active, but the floating space should render on top, even if the
    /// active scrolling window is fullscreen.
    ///
    /// This is necessary for focus-follows-mouse that activates but doesn't raise the window to
    /// avoid being annoying.
    NoButRaised,
    /// The floating space is active.
    Yes,
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// As a new column at this index.
    NewColumnAt(usize),
    /// Next to this existing window.
    NextTo(&'a W::Id),
}

impl FloatingActive {
    fn get(self) -> bool {
        self == Self::Yes
    }
}

impl<W: LayoutElement> Workspace<W> {
    pub fn new(output: Output, clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config(output, None, clock, options)
    }

    pub fn new_with_config(
        output: Output,
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = config
            .as_ref()
            .and_then(|c| c.open_on_output.clone())
            .map(OutputIdentity::from_connector)
            .unwrap_or_else(|| OutputIdentity::from_output(&output));

        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = output.current_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = output_size(&output);
        let working_area = compute_working_area(&output, &options);

        let scrolling = ScrollingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            scrolling,
            floating,
            floating_is_active: FloatingActive::No,
            minimized: Vec::new(),
            original_output,
            home_ordinal: 0,
            claimed: false,
            scale,
            transform: output.current_transform(),
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            output: Some(output),
            clock,
            base_options,
            options,
            name: config.map(|c| c.name.0),
            layout_config,
            expose_reserved: None,
            expose_freeze: None,
            expose_retained: RefCell::new(None),
            expose_recomputes: Cell::new(0),
            expose_slides: Vec::new(),
            expose_hover: Vec::new(),
            id: WorkspaceId::next(),
        }
    }

    pub fn new_with_config_no_outputs(
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
        base_options: Rc<Options>,
    ) -> Self {
        // No output to take an identity from, so the connector is whatever was configured — and
        // the empty one when nothing was, which names no display and matches none.
        let original_output = OutputIdentity::from_connector(
            config
                .as_ref()
                .and_then(|c| c.open_on_output.clone())
                .unwrap_or_default(),
        );

        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = smithay::output::Scale::Integer(1);
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = Size::from((1280., 720.));
        let working_area = Rectangle::from_size(Size::from((1280., 720.)));

        let scrolling = ScrollingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            scrolling,
            floating,
            floating_is_active: FloatingActive::No,
            minimized: Vec::new(),
            output: None,
            scale,
            transform: Transform::Normal,
            original_output,
            home_ordinal: 0,
            claimed: false,
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            clock,
            base_options,
            options,
            name: config.map(|c| c.name.0),
            layout_config,
            expose_reserved: None,
            expose_freeze: None,
            expose_retained: RefCell::new(None),
            expose_recomputes: Cell::new(0),
            expose_slides: Vec::new(),
            expose_hover: Vec::new(),
            id: WorkspaceId::next(),
        }
    }

    pub fn new_no_outputs(clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config_no_outputs(None, clock, options)
    }

    /// Makes an unclaimed workspace this output's own, as putting a window on it does.
    ///
    /// A workspace *visiting* from a display that is away keeps the home it has: a window landing
    /// on it is not an explicit move, and taking the workspace away from its display for it would
    /// leave the display nothing to come back to (`docs/fork/multi-display.md` §2). What is left
    /// is the workspace that never had a home — one made while no display was connected — and
    /// filling in the EDID for one whose home is this display under a bare connector name.
    pub(super) fn adopt_home(&mut self, output: &Output) {
        if self.original_output.connector.is_empty() || self.original_output.matches_output(output)
        {
            self.original_output = OutputIdentity::from_output(output);
        }
    }

    /// Makes this workspace `home`'s, at `home_ordinal` in its strip.
    ///
    /// Session restore's one caller: a workspace it materializes for a display that is not here
    /// belongs to that display, so plugging the display in reclaims it, and the saved index it
    /// carries is what tells the reclaim where in the arrangement it goes.
    ///
    /// Claiming also takes the workspace out of the strip's filler. It holds no window until the
    /// one it was made for finishes mapping, and until then it is indistinguishable from an empty
    /// the invariants grew — which would let the strip drop it, or sort it as if the desktop the
    /// user saved were not spoken for.
    pub(super) fn claim_for(&mut self, home: OutputIdentity, home_ordinal: usize) -> WorkspaceId {
        self.original_output = home;
        self.home_ordinal = home_ordinal;
        self.claimed = true;
        self.id()
    }

    /// Whether the strip means something by this workspace: a window, a name, or a saved record
    /// whose window is still on its way. The rest is filler the invariants keep around.
    pub(super) fn is_substantial(&self) -> bool {
        self.has_windows_or_name() || self.claimed
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn name(&self) -> Option<&String> {
        self.name.as_ref()
    }

    pub fn unname(&mut self) {
        self.name = None;
    }

    pub fn has_windows_or_name(&self) -> bool {
        self.has_windows() || self.name.is_some()
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn advance_animations(&mut self) {
        self.scrolling.advance_animations();
        self.floating.advance_animations();

        // A landed shrink stops being drawn. The window stays parked either way: the animation
        // is how it leaves the screen, never whether it is minimized.
        //
        // Parked tiles are advanced here because they are in neither half: nothing else walks
        // them. Without this the shrink's fade would never be cleared, and the picker would draw
        // every minimized preview at the alpha the shrink ended on — zero.
        for parked in &mut self.minimized {
            if parked.shrink.as_ref().is_some_and(|s| s.anim.is_done()) {
                parked.shrink = None;
            }
            parked.removed.tile.advance_animations();
        }

        // The pointer has held still long enough — gnome-shell's tick at
        // `workspace.js:1164-1179`. A pointer resting on one of our own previews holds it
        // open anyway, whether or not it has moved: that is the second disjunct at `:1170`,
        // and the reason the freeze exists at all, since a close button has to stay under the
        // cursor for a second click. Checked here rather than only on motion, because a close
        // that is never followed by any motion would otherwise reflow under a still pointer.
        if self
            .expose_freeze
            .as_ref()
            .is_some_and(|f| f.hold.as_ref().is_some_and(Animation::is_done))
        {
            if self.expose_hovers_a_live_preview() {
                self.expose_freeze.as_mut().unwrap().hold = None;
            } else {
                self.release_expose_freeze();
            }
        }

        self.expose_hover
            .retain(|(_, anim)| !(anim.is_done() && anim.to() == 0.));
        // A slot ease that has landed is over: the preview is at the slot the picker
        // computes for it, which is what it would draw at anyway.
        self.expose_slides
            .retain(|(_, slide)| !slide.anim.is_done());
    }

    pub fn are_animations_ongoing(&self) -> bool {
        // A timed hold has to be noticed by a frame to run out, so it asks for them. An
        // indefinite one deliberately does not — see [`CloseFreeze::hold`].
        self.expose_freeze
            .as_ref()
            .is_some_and(|f| f.hold.is_some())
            || self.scrolling.are_animations_ongoing()
            || self.floating.are_animations_ongoing()
            || self.expose_hover.iter().any(|(_, anim)| !anim.is_done())
            || self
                .expose_slides
                .iter()
                .any(|(_, slide)| !slide.anim.is_done())
            || self.minimized.iter().any(|parked| {
                parked.shrink.is_some() || parked.removed.tile.are_animations_ongoing()
            })
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.scrolling.are_transitions_ongoing() || self.floating.are_transitions_ongoing()
    }

    /// `background_radius` is the corner radius the workspace background is
    /// rounded to (see `Monitor::workspace_background_radius`). The shadow has to
    /// use the *same* one: gnome-shell's `.workspace-background` carries its
    /// `box-shadow` on the same rounded box (`_window-picker.scss:56-60`), and a
    /// square-cornered shadow around a rounded background leaves the backdrop
    /// showing through each corner as a pointy tab.
    pub fn update_render_elements(&mut self, is_active: bool, background_radius: f64) {
        self.scrolling
            .update_render_elements(is_active && !self.floating_is_active.get());

        let view_rect = Rectangle::from_size(self.view_size);
        self.floating
            .update_render_elements(is_active && self.floating_is_active.get(), view_rect);

        self.shadow.update_render_elements(
            self.view_size,
            true,
            CornerRadius::from(background_radius as f32),
            self.scale.fractional_scale(),
            1.,
        );
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let scale = self.scale.fractional_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(self.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );

        self.scrolling.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, self.view_size);
        self.shadow.update_config(shadow_config);

        self.background_buffer
            .set_color(options.layout.background_color);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<synoik_config::LayoutPart>) {
        if self.layout_config == layout_config {
            return;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());
    }

    pub fn update_shaders(&mut self) {
        self.scrolling.update_shaders();
        self.floating.update_shaders();
        self.shadow.update_shaders();
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    /// Every tile the workspace holds, **including the minimized ones**.
    ///
    /// This is the "which windows are on this workspace" question — `Workspace::windows`,
    /// `has_window`, `has_windows_or_name` and the by-id lookups all come through here, and a
    /// minimized window is still on its workspace (it is hidden, not gone). For "what is drawn
    /// and what can be clicked", use [`Self::tiles_with_render_positions`].
    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles();
        let floating = self.floating.tiles();
        let minimized = self.minimized.iter().map(|parked| &parked.removed.tile);
        scrolling.chain(floating).chain(minimized)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles_mut();
        let floating = self.floating.tiles_mut();
        let minimized = self
            .minimized
            .iter_mut()
            .map(|parked| &mut parked.removed.tile);
        scrolling.chain(floating).chain(minimized)
    }

    /// Whether `id` is minimized on this workspace.
    pub fn is_minimized(&self, id: &W::Id) -> bool {
        self.minimized
            .iter()
            .any(|parked| parked.removed.tile.window().id() == id)
    }

    /// Take `id` out of whichever layout holds it and park it — `meta_window_minimize`
    /// (`window.c:2734-2771`). Returns whether anything changed; a window already minimized, or
    /// not on this workspace at all, is a no-op like mutter's `if (!window->minimized)` guard.
    ///
    /// The focus fixup rides [`Self::remove_tile`], which is the whole reason minimizing goes
    /// through a real removal rather than a flag the arithmetic skips: a flag would leave the
    /// focus on an invisible window, because none of the index fixups in the scrolling and
    /// floating halves would run.
    /// `dest` is where the hidden window lives: the app's dock icon when the shell could name one,
    /// else the dock's home edge ([`Layout::minimize_window`] resolves it), in workspace
    /// coordinates. The window shrinks into it, and the picker later grows the preview back out
    /// of it — one rect, so the two motions cannot disagree about where the window went.
    ///
    /// `animate` is whether the *desktop* shrink runs. It is a separate question: a window that
    /// was never on screen has nowhere to shrink from, but still has to grow out of the right
    /// place when the overview opens.
    pub fn minimize(
        &mut self,
        id: &W::Id,
        transaction: Transaction,
        dest: Rectangle<f64, Logical>,
        animate: bool,
    ) -> bool {
        if self.is_minimized(id) {
            return false;
        }
        match self.home_of(id) {
            Some(Home::Floating) | Some(Home::Scrolling) => (),
            // Already parked (handled above) or not ours.
            Some(Home::Minimized) | None => return false,
        }

        // Taken before the removal: afterwards the tile has no place to have come from.
        let from = self
            .tiles_with_render_positions()
            .find(|(tile, _, _)| tile.window().id() == id)
            .map(|(tile, pos, _)| Rectangle::new(pos, tile.tile_size()));

        let dest = sane_dest(dest);
        let mut removed = self.remove_tile(id, transaction);
        removed.tile.window_mut().set_minimized(true);
        let shrink = from.filter(|_| animate).map(|from| Shrink {
            from,
            anim: Animation::ease(
                self.clock.clone(),
                0.,
                1.,
                0.,
                MINIMIZE_ANIMATION_MS,
                Curve::EaseOutExpo,
            ),
        });
        if shrink.is_some() {
            // The other half of gnome-shell's `_minimizeWindow`, which eases scale, position and
            // opacity together (`windowManager.js:1198-1208`). A parked tile is outside both
            // layout halves, so a non-1 alpha target here cannot trip the visible-tile invariant.
            removed
                .tile
                .animate_alpha(1., 0., minimize_animation_config());
        }
        self.minimized.push(Parked {
            removed,
            dest,
            shrink,
        });
        true
    }

    /// Put a minimized window back where it came from — `meta_window_unminimize`
    /// (`window.c:2773-2790`). Returns whether anything changed.
    pub fn unminimize(&mut self, id: &W::Id, activate: ActivateWindow) -> bool {
        let Some(idx) = self
            .minimized
            .iter()
            .position(|parked| parked.removed.tile.window().id() == id)
        else {
            return false;
        };
        let Parked { removed, dest, .. } = self.minimized.remove(idx);
        let RemovedTile {
            mut tile,
            width,
            is_full_width,
            is_floating,
        } = removed;
        tile.window_mut().set_minimized(false);
        // The shrink run backwards, which is what gnome-shell's `_unminimizeWindow` is: the same
        // ease, from the same icon geometry (`windowManager.js:1222-1260`). Growth and fade are
        // started before the add because neither needs to know where the tile lands — the grow
        // reads its target position fresh from the render each frame.
        tile.animate_grow_from(dest, minimize_animation_config());
        tile.animate_alpha(0., 1., minimize_animation_config());
        self.add_tile(
            tile,
            WorkspaceAddWindowTarget::Auto,
            activate,
            width,
            is_full_width,
            is_floating,
        );
        true
    }

    /// Set or clear always-on-top — see [`FloatingSpace::set_above`].
    ///
    /// Floating only, and that is not a gap: in GNOME mode every window is floating, and niri's
    /// scrolling layout has no stacking order for a window to be on top *of*.
    pub fn set_above(&mut self, id: &W::Id, above: bool) -> bool {
        self.floating.set_above(id, above)
    }

    /// Whether `id` is flagged always-on-top — see [`FloatingSpace::is_above`].
    pub fn is_above(&self, id: &W::Id) -> bool {
        self.floating.is_above(id)
    }

    /// See [`FloatingSpace::lower_window`].
    pub fn lower_window(&mut self, id: &W::Id) -> bool {
        self.floating.lower_window(id)
    }

    /// See [`FloatingSpace::raise_or_lower`].
    pub fn raise_or_lower(&mut self, id: &W::Id) -> bool {
        self.floating.raise_or_lower(id)
    }

    /// Whether `id` carries the Always on Visible Workspace flag itself. For "does it ride a
    /// workspace switch", which also covers its dialogs, see [`Self::sticky_window_ids`].
    pub fn is_sticky(&self, id: &W::Id) -> bool {
        self.tiles()
            .any(|tile| tile.window().id() == id && tile.is_sticky)
    }

    /// The workspace `id` was on when it was stuck, if it still remembers one.
    pub fn sticky_home(&self, id: &W::Id) -> Option<WorkspaceId> {
        self.tiles()
            .find(|tile| tile.window().id() == id)
            .and_then(|tile| tile.sticky_home)
    }

    /// Set or clear the flag, recording this workspace as the home to revert to. Returns whether
    /// anything changed.
    pub fn set_sticky(&mut self, id: &W::Id, sticky: bool) -> bool {
        let home = self.id();
        let Some(tile) = self.tiles_mut().find(|tile| tile.window().id() == id) else {
            return false;
        };
        if tile.is_sticky == sticky {
            return false;
        }
        tile.is_sticky = sticky;
        if sticky {
            tile.sticky_home = Some(home);
        }
        true
    }

    /// The windows that ride a workspace switch: flagged, or descended from a flagged one.
    ///
    /// Derived rather than stored on each transient the way mutter does (`stick_foreach_func`),
    /// so a dialog that maps *after* its parent was stuck is sticky too. Minimized windows are
    /// left out: they are parked rather than laid out, and there is nothing on screen to carry.
    pub fn sticky_window_ids(&self) -> Vec<W::Id> {
        let tiles: Vec<&Tile<W>> = self.tiles().collect();
        let mut ids = Vec::new();
        for (idx, tile) in tiles.iter().enumerate() {
            let mut current = idx;
            for _ in 0..tiles.len() {
                if tiles[current].is_sticky {
                    if self.has_window(tile.window().id()) {
                        ids.push(tile.window().id().clone());
                    }
                    break;
                }
                let win = tiles[current].window();
                let parent = (0..tiles.len())
                    .find(|&other| other != current && win.is_child_of(tiles[other].window()));
                match parent {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
        }
        ids
    }

    /// See [`FloatingSpace::nudge_window`].
    pub fn nudge_window(&mut self, id: &W::Id, amount: Point<f64, Logical>) -> bool {
        self.floating.nudge_window(id, amount)
    }

    /// See [`FloatingSpace::raise_window_only`].
    pub fn raise_window_only(&mut self, id: &W::Id) -> bool {
        self.floating.raise_window_only(id)
    }

    /// See [`FloatingSpace::set_preview_raised`].
    pub fn set_preview_raised(&mut self, ids: &[W::Id]) {
        self.floating.set_preview_raised(ids);
    }

    /// See [`FloatingSpace::preview_raised`].
    pub fn preview_raised(&self) -> &[W::Id] {
        self.floating.preview_raised()
    }

    pub fn is_floating(&self, id: &W::Id) -> bool {
        self.home_of(id) == Some(Home::Floating)
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.floating_is_active.get() {
            self.floating.active_window()
        } else {
            self.scrolling.active_window()
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            self.floating.active_window_mut()
        } else {
            self.scrolling.active_window_mut()
        }
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.scrolling.is_active_pending_fullscreen()
    }

    pub fn set_output(&mut self, output: Option<Output>) {
        if self.output == output {
            return;
        }

        if let Some(output) = self.output.take() {
            for win in self.windows() {
                win.output_leave(&output);
            }
        }

        self.output = output;

        if let Some(output) = &self.output {
            // A home tag that came from configuration carries a connector and no EDID. Meeting
            // its display fills the rest in, which is what makes the veto mean anything afterwards.
            if self.original_output.matches_output(output) {
                self.original_output = OutputIdentity::from_output(output);
            }

            self.update_output_size();

            for win in self.windows() {
                self.enter_output_for_window(win);
            }
        }
    }

    fn enter_output_for_window(&self, window: &W) {
        if let Some(output) = &self.output {
            window.set_preferred_scale_transform(self.scale, self.transform);
            window.output_enter(output);
        }
    }

    pub fn update_output_size(&mut self) {
        let output = self.output.as_ref().unwrap();
        let scale = output.current_scale();
        let transform = output.current_transform();
        let view_size = output_size(output);
        let working_area = compute_working_area(output, &self.options);
        self.set_view_size(scale, transform, view_size, working_area);
    }

    fn set_view_size(
        &mut self,
        scale: smithay::output::Scale,
        transform: Transform,
        size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let scale_transform_changed = self.transform != transform
            || self.scale.integer_scale() != scale.integer_scale()
            || self.scale.fractional_scale() != scale.fractional_scale();
        if !scale_transform_changed && self.view_size == size && self.working_area == working_area {
            return;
        }

        let fractional_scale_changed = self.scale.fractional_scale() != scale.fractional_scale();

        self.scale = scale;
        self.transform = transform;
        self.view_size = size;
        self.working_area = working_area;

        if fractional_scale_changed {
            // Options need to be recomputed for the new scale.
            self.update_config(self.base_options.clone());
        } else {
            // Pass our existing options as is.
            self.scrolling.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );

            let shadow_config =
                compute_workspace_shadow_config(self.options.overview.workspace_shadow, size);
            self.shadow.update_config(shadow_config);
        }

        self.background_buffer.resize(size);

        if scale_transform_changed {
            for window in self.windows() {
                window.set_preferred_scale_transform(self.scale, self.transform);
            }
        }
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn make_tile(&self, window: W) -> Tile<W> {
        Tile::new(
            window,
            self.view_size,
            self.scale.fractional_scale(),
            self.clock.clone(),
            self.options.clone(),
        )
    }

    pub fn add_tile(
        &mut self,
        mut tile: Tile<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        self.release_expose_freeze_on_arrival();
        self.enter_output_for_window(tile.window());

        // GNOME windowing has a single layer: windows stack, they never tile into columns. Its
        // maximized and fullscreen windows are sized by the floating space itself (see
        // `FloatingSpace::set_maximized`), so nothing is ever routed into the scrolling layout —
        // side-by-side columns and the horizontal view pan between them are niri's model, and on
        // this desktop what lies to the side of a workspace is another workspace.
        //
        // In niri's scrolling mode only the scrolling layout can size a window to the screen, so a
        // window that opens maximized or fullscreen goes there regardless of `is_floating`.
        // A `NextTo` reference has to be somewhere with a position: a minimized window is in
        // neither half, and one this workspace does not hold was never a position here either.
        // Normalizing to `Auto` up front is what keeps the placement below reading a plain
        // floating-or-scrolling question.
        let target = match target {
            WorkspaceAddWindowTarget::NextTo(next_to) => match self.home_of(next_to) {
                Some(Home::Floating) | Some(Home::Scrolling) => {
                    WorkspaceAddWindowTarget::NextTo(next_to)
                }
                Some(Home::Minimized) | None => WorkspaceAddWindowTarget::Auto,
            },
            other => other,
        };

        let gnome_mode = self.options.layout.windowing_mode == WindowingMode::Floating;
        let is_floating = is_floating || gnome_mode;
        let opens_floating =
            is_floating && (gnome_mode || tile.window().pending_sizing_mode().is_normal());

        tile.restore_to_floating = is_floating;

        match target {
            WorkspaceAddWindowTarget::Auto => {
                // Don't steal focus from an active fullscreen window.
                let activate = activate.map_smart(|| !self.is_active_pending_fullscreen());

                if opens_floating {
                    self.floating.add_tile(tile, activate);

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else {
                    self.scrolling
                        .add_tile(None, tile, activate, width, is_full_width, None);

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
            // Placing a window in a specific column is a scrolling-layout notion; in GNOME mode
            // there are no columns, so it just opens on the stack.
            WorkspaceAddWindowTarget::NewColumnAt(_) if gnome_mode => {
                let activate = activate.map_smart(|| false);
                self.floating.add_tile(tile, activate);

                if activate {
                    self.floating_is_active = FloatingActive::Yes;
                }
            }
            WorkspaceAddWindowTarget::NewColumnAt(col_idx) => {
                let activate = activate.map_smart(|| false);
                self.scrolling
                    .add_tile(Some(col_idx), tile, activate, width, is_full_width, None);

                if activate {
                    self.floating_is_active = FloatingActive::No;
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                let activate = activate.map_smart(|| self.active_window().unwrap().id() == next_to);

                // Normalized above, so `next_to` is known to be laid out in one half or the
                // other and this boolean cannot be reading a third state as the second.
                let floating_has_window = self.floating.has_window(next_to);

                if opens_floating {
                    if floating_has_window {
                        self.floating.add_tile_above(next_to, tile, activate);
                    } else {
                        // FIXME: use static pos
                        let (next_to_tile, render_pos, _visible) = self
                            .scrolling
                            .tiles_with_render_positions()
                            .find(|(tile, _, _)| tile.window().id() == next_to)
                            .unwrap();

                        // Position the new tile in the center above the next_to tile. Think a
                        // dialog opening on top of a window.
                        let tile_size = tile.tile_size();
                        let pos = render_pos
                            + (next_to_tile.tile_size().to_point() - tile_size.to_point())
                                .downscale(2.);
                        let pos = self.floating.clamp_within_working_area(pos, tile_size);
                        let pos = self.floating.logical_to_size_frac(pos);
                        tile.floating_pos = Some(pos);

                        self.floating.add_tile(tile, activate);
                    }

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else if floating_has_window {
                    self.scrolling
                        .add_tile(None, tile, activate, width, is_full_width, None);

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                } else {
                    self.scrolling
                        .add_tile_right_of(next_to, tile, activate, width, is_full_width);

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.release_expose_freeze_on_arrival();
        self.enter_output_for_window(tile.window());
        self.scrolling
            .add_tile_to_column(col_idx, tile_idx, tile, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    pub fn add_column(&mut self, column: Column<W>, activate: bool) {
        self.release_expose_freeze_on_arrival();
        for (tile, _) in column.tiles() {
            self.enter_output_for_window(tile.window());
        }

        self.scrolling.add_column(None, column, activate, None);

        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    fn update_focus_floating_tiling_after_removing(&mut self, removed_from_floating: bool) {
        if removed_from_floating {
            if self.floating.is_empty() {
                self.floating_is_active = FloatingActive::No;
            }
        } else {
            // Scrolling should remain focused if both are empty.
            if self.scrolling.is_empty() && !self.floating.is_empty() {
                self.floating_is_active = FloatingActive::Yes;
            }
        }
    }

    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        // A minimized window is still ours, so its client can still close it. It is already out
        // of both layouts, so there is no arithmetic to fix up and no focus to move.
        if let Some(idx) = self
            .minimized
            .iter()
            .position(|parked| parked.removed.tile.window().id() == id)
        {
            let mut removed = self.minimized.remove(idx).removed;
            removed.tile.window_mut().set_minimized(false);
            if let Some(output) = &self.output {
                removed.tile.window().output_leave(output);
            }
            return removed;
        }

        let mut from_floating = false;
        let removed = match self.home_of(id) {
            Some(Home::Floating) => {
                from_floating = true;
                self.floating.remove_tile(id)
            }
            Some(Home::Scrolling) => self.scrolling.remove_tile(id, transaction),
            // Handled above, and a window the workspace does not hold cannot be removed from it.
            Some(Home::Minimized) | None => {
                panic!("removing a window this workspace does not lay out")
            }
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        // The pointer is very often still on the preview that just went away — a close button
        // click is exactly that — and nothing else clears the entry until the next motion. A
        // corpse left in there reads as "the pointer is on a preview" to the freeze, which
        // would hold the picker until the user moved the mouse.
        self.expose_hover.retain(|(other, _)| other != id);

        self.update_focus_floating_tiling_after_removing(from_floating);

        removed
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let from_floating = self.floating_is_active.get();
        let removed = if from_floating {
            self.floating.remove_active_tile()?
        } else {
            self.scrolling.remove_active_tile(transaction)?
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        // See [`Self::remove_tile`].
        let id = removed.tile.window().id().clone();
        self.expose_hover.retain(|(other, _)| *other != id);

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(removed)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        let from_floating = self.floating_is_active.get();
        if from_floating {
            return None;
        }

        let column = self.scrolling.remove_active_column()?;

        if let Some(output) = &self.output {
            for (tile, _) in column.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(column)
    }

    pub fn resolve_default_width(
        &self,
        default_width: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_width {
            Some(Some(width)) => Some(width),
            Some(None) => None,
            None if is_floating => None,
            None => self.options.layout.default_column_width,
        }
    }

    pub fn resolve_default_height(
        &self,
        default_height: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_height {
            Some(Some(height)) => Some(height),
            Some(None) => None,
            None if is_floating => None,
            // We don't have a global default at the moment.
            None => None,
        }
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
        (min_size, max_size): (Size<i32, Logical>, Size<i32, Logical>),
    ) -> Size<i32, Logical> {
        let mut size = if is_floating {
            self.floating.new_window_size(width, height, rules)
        } else {
            self.scrolling.new_window_size(width, height, rules)
        };

        // If the window has a fixed size, or we're picking some fixed size, apply min and max
        // size. This is to ensure that a fixed-size window rule works on open, while still
        // allowing the window freedom to pick its default size otherwise.
        let (min_size, max_size) = rules.apply_min_max_size(min_size, max_size);
        size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        // For scrolling (where height is > 0) only ensure fixed height, since at runtime scrolling
        // will only honor fixed height currently.
        if min_size.h == max_size.h {
            size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);
        } else if size.h > 0 {
            // Also always honor min height, scrolling always does.
            size.h = max(size.h, min_size.h);
        }

        size
    }

    pub fn configure_new_window(
        &self,
        window: &Window,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
    ) {
        window.with_surfaces(|surface, data| {
            send_scale_transform(surface, data, self.scale, self.transform);
        });

        let toplevel = window.toplevel().expect("no x11 support");
        let (min_size, max_size) = with_states(toplevel.wl_surface(), |state| {
            let mut guard = state.cached_state.get::<SurfaceCachedState>();
            let current = guard.current();
            (current.min_size, current.max_size)
        });
        toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.size = Some(self.view_size.to_i32_round());
            } else if state.states.contains(xdg_toplevel::State::Maximized) {
                state.size = Some(self.working_area.size.to_i32_round());
            } else {
                let size =
                    self.new_window_size(width, height, is_floating, rules, (min_size, max_size));
                state.size = Some(size);
            }

            if is_floating {
                state.bounds = Some(self.floating.new_window_toplevel_bounds(rules));
            } else {
                state.bounds = Some(self.scrolling.new_window_toplevel_bounds(rules));
            }
        });
    }

    pub(super) fn resolve_scrolling_width(
        &self,
        window: &W,
        width: Option<PresetSize>,
    ) -> ColumnWidth {
        let width = width.unwrap_or_else(|| PresetSize::Fixed(window.size().w));
        match width {
            PresetSize::Fixed(fixed) => {
                let mut fixed = f64::from(fixed);

                // Add border width since ColumnWidth includes borders.
                let rules = window.rules();
                let border = self.options.layout.border.merged_with(&rules.border);
                if !border.off {
                    fixed += border.width * 2.;
                }

                ColumnWidth::Fixed(fixed)
            }
            PresetSize::Proportion(prop) => ColumnWidth::Proportion(prop),
        }
    }

    pub fn focus_left(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_left()
        } else {
            self.scrolling.focus_left()
        }
    }

    pub fn focus_right(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_right()
        } else {
            self.scrolling.focus_right()
        }
    }

    pub fn focus_column_first(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_leftmost();
        } else {
            self.scrolling.focus_column_first();
        }
    }

    pub fn focus_column_last(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_rightmost();
        } else {
            self.scrolling.focus_column_last();
        }
    }

    pub fn focus_column_right_or_first(&mut self) {
        if !self.focus_right() {
            self.focus_column_first();
        }
    }

    pub fn focus_column_left_or_last(&mut self) {
        if !self.focus_left() {
            self.focus_column_last();
        }
    }

    pub fn focus_column(&mut self, index: usize) {
        if self.floating_is_active.get() {
            self.focus_tiling();
        }
        self.scrolling.focus_column(index);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.focus_window_in_column(index);
    }

    pub fn focus_down(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_down()
        } else {
            self.scrolling.focus_down()
        }
    }

    pub fn focus_up(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_up()
        } else {
            self.scrolling.focus_up()
        }
    }

    pub fn focus_down_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            self.scrolling.focus_down_or_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            self.scrolling.focus_down_or_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            self.scrolling.focus_up_or_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            self.scrolling.focus_up_or_right();
        }
    }

    pub fn focus_window_top(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_topmost();
        } else {
            self.scrolling.focus_top();
        }
    }

    pub fn focus_window_bottom(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_bottommost();
        } else {
            self.scrolling.focus_bottom();
        }
    }

    pub fn focus_window_down_or_top(&mut self) {
        if !self.focus_down() {
            self.focus_window_top();
        }
    }

    pub fn focus_window_up_or_bottom(&mut self) {
        if !self.focus_up() {
            self.focus_window_bottom();
        }
    }

    pub fn move_left(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_left();
            true
        } else {
            self.scrolling.move_left()
        }
    }

    pub fn move_right(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_right();
            true
        } else {
            self.scrolling.move_right()
        }
    }

    pub fn move_column_to_first(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_last();
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_index(index);
    }

    pub fn move_down(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_down();
            true
        } else {
            self.scrolling.move_down()
        }
    }

    pub fn move_up(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_up();
            true
        } else {
            self.scrolling.move_up()
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        match self.target_home(window) {
            Some(Home::Scrolling) => self.scrolling.consume_or_expel_window_left(window),
            // Columns are the scrolling half's idea; there is nothing to consume into or expel
            // from for a floating window, and nothing at all for a hidden one.
            Some(Home::Floating) | Some(Home::Minimized) | None => (),
        }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        match self.target_home(window) {
            Some(Home::Scrolling) => self.scrolling.consume_or_expel_window_right(window),
            Some(Home::Floating) | Some(Home::Minimized) | None => (),
        }
    }

    pub fn consume_into_column(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.consume_into_column();
    }

    pub fn expel_from_column(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.expel_from_column();
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.swap_window_in_direction(direction);
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.toggle_column_tabbed_display();
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.set_column_display(display);
    }

    pub fn center_column(&mut self) {
        if self.floating_is_active.get() {
            self.floating.center_window(None);
        } else {
            self.scrolling.center_column();
        }
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        match self.target_home(id) {
            Some(Home::Floating) => self.floating.center_window(id),
            Some(Home::Scrolling) => self.scrolling.center_window(id),
            Some(Home::Minimized) | None => (),
        }
    }

    pub fn center_visible_columns(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.center_visible_columns();
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        if self.floating_is_active.get() {
            self.floating.toggle_window_width(None, forwards);
        } else {
            self.scrolling.toggle_width(forwards);
        }
    }

    pub fn toggle_full_width(&mut self) {
        if self.floating_is_active.get() {
            // Leave this unimplemented for now. For good UX, this probably needs moving the tile
            // to be against the left edge of the working area while it is full-width.
            return;
        }
        self.scrolling.toggle_full_width();
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        if self.floating_is_active.get() {
            self.floating.set_window_width(None, change, true);
        } else {
            self.scrolling.set_window_width(None, change);
        }
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        match self.target_home(window) {
            Some(Home::Floating) => self.floating.set_window_width(window, change, true),
            Some(Home::Scrolling) => self.scrolling.set_window_width(window, change),
            Some(Home::Minimized) | None => (),
        }
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        match self.target_home(window) {
            Some(Home::Floating) => self.floating.set_window_height(window, change, true),
            Some(Home::Scrolling) => self.scrolling.set_window_height(window, change),
            Some(Home::Minimized) | None => (),
        }
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        match self.target_home(window) {
            Some(Home::Scrolling) => self.scrolling.reset_window_height(window),
            // A floating window has no column height to reset.
            Some(Home::Floating) | Some(Home::Minimized) | None => (),
        }
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        match self.target_home(window) {
            Some(Home::Floating) => self.floating.toggle_window_width(window, forwards),
            Some(Home::Scrolling) => self.scrolling.toggle_window_width(window, forwards),
            Some(Home::Minimized) | None => (),
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        match self.target_home(window) {
            Some(Home::Floating) => self.floating.toggle_window_height(window, forwards),
            Some(Home::Scrolling) => self.scrolling.toggle_window_height(window, forwards),
            Some(Home::Minimized) | None => (),
        }
    }

    pub fn expand_column_to_available_width(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.expand_column_to_available_width();
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        // GNOME mode: every window lives in the floating layout, which sizes fullscreen windows
        // itself. No migration into a scrolling column, so no column to be side by side with.
        if self.options.layout.windowing_mode == WindowingMode::Floating {
            self.floating.set_fullscreen(window, is_fullscreen);
            return;
        }

        let mut restore_to_floating = false;
        // Like maximize: fullscreening an edge-tiled window carries the
        // pre-tile rect as the restore rect.
        let mut tiled_restore = None;
        // Not a two-way dispatch below but a chain, so the third state is a guard rather than an
        // arm — still exhaustive, so a new mode still has to answer here.
        match self.home_of(window) {
            Some(Home::Floating) | Some(Home::Scrolling) => (),
            // A hidden window has no geometry to size.
            Some(Home::Minimized) | None => return,
        }
        if self.floating.has_window(window) {
            if is_fullscreen {
                tiled_restore = self.floating.take_tile_restore(window);
                restore_to_floating = true;
                self.toggle_window_floating(Some(window));
            } else {
                // Floating windows are never fullscreen, so this is an unfullscreen request for an
                // already unfullscreen window.
                return;
            }
        } else if !is_fullscreen {
            // The window is in the scrolling layout and we're requesting an unfullscreen. If it is
            // indeed fullscreen (i.e. this isn't a duplicate unfullscreen request), then we may
            // need to unfullscreen into floating.
            let col = self
                .scrolling
                .columns()
                .find(|col| col.contains(window))
                .unwrap();

            // When going from fullscreen to maximized, don't consider restore_to_floating yet.
            if col.is_pending_fullscreen() && !col.is_pending_maximized() {
                let (tile, _) = col
                    .tiles()
                    .find(|(tile, _)| tile.window().id() == window)
                    .unwrap();
                if tile.restore_to_floating {
                    // Unfullscreen and float in one call so it has a chance to notice and request a
                    // (0, 0) size, rather than the scrolling column size.
                    self.toggle_window_floating(Some(window));
                    return;
                }
            }
        }

        let tile = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.scrolling.set_fullscreen(window, is_fullscreen);

        // When going from normal to fullscreen, remember if we should unfullscreen to floating.
        let tile = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && !tile.window().pending_sizing_mode().is_normal() {
            tile.restore_to_floating = restore_to_floating;
            if let Some((size, pos)) = tiled_restore {
                if size.is_some() {
                    tile.floating_window_size = size;
                }
                if pos.is_some() {
                    tile.floating_pos = pos;
                }
            }
        }
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        let tile = self
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let current = tile.window().pending_sizing_mode().is_fullscreen();
        self.set_fullscreen(window, !current);
    }

    /// Tiles the window to the given half of the work area, or untiles it if
    /// already tiled there (GNOME Super+Left/Right).
    pub fn toggle_tiled(&mut self, window: Option<&W::Id>, side: TileSide) {
        let id = window
            .or_else(|| self.active_window().map(|win| win.id()))
            .cloned();
        let Some(id) = id else {
            return;
        };

        // A maximized window tiles from its maximized state (mutter clears the maximization and
        // tiles from the pre-maximize rect); in GNOME mode it never left the floating layout, so
        // `toggle_tiled` handles it directly.
        match self.home_of(&id) {
            Some(Home::Floating) => {
                self.floating.toggle_tiled(Some(&id), side);
                return;
            }
            // Handled below: a scrolling-half maximized window comes back to floating first.
            Some(Home::Scrolling) => (),
            // A hidden window has no edge to tile to.
            Some(Home::Minimized) | None => return,
        }

        // In scrolling mode a maximized window lives in the scrolling layout: bring it back to
        // floating, then tile. (mutter would remember to restore to maximized on untile —
        // saved_maximize — which we don't.)
        let is_maximized_floater = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == &id)
            .is_some_and(|tile| {
                tile.window().pending_sizing_mode().is_maximized() && tile.restore_to_floating
            });
        if is_maximized_floater {
            self.set_maximized(&id, false);
            if self.floating.has_window(&id) {
                self.floating.toggle_tiled(Some(&id), side);
            }
        }
    }

    /// Restores an edge-tiled window, see [`FloatingSpace::restore_edge_tiled`].
    ///
    /// Only the floating layer can hold an edge-tiled window. In niri's scrolling mode a restored
    /// window may have opened into the scrolling layout instead, which owns its size; there is no
    /// tiling to restore there, so the state is dropped rather than approximated.
    pub fn restore_edge_tiled(&mut self, window: &W::Id, side: TileSide) {
        match self.home_of(window) {
            Some(Home::Floating) => self.floating.restore_edge_tiled(window, side),
            Some(Home::Scrolling) => (),
            // A hidden window has no edge to tile to; a restore that also asked for minimize
            // applies it after this, so the tiling lands first and the window is hidden at the
            // size it would have had.
            Some(Home::Minimized) | None => (),
        }
    }

    /// mutter's denied-focus placement, see
    /// [`FloatingSpace::avoid_focus_window`].
    pub fn avoid_focus_window(&mut self, window: &W::Id, focus: &W::Id) -> bool {
        self.floating.avoid_focus_window(window, focus)
    }

    /// mutter's map-time auto-maximize (place.c): a window covering more
    /// than 80% of the work area opens maximized. The restore size is
    /// clamped to sqrt(0.8) of the work area per dimension, aspect
    /// preserved (mutter clamps in set_unmaximize_flags instead; same
    /// outcome for this path).
    pub fn auto_maximize_if_too_big(&mut self, window: &W::Id) -> bool {
        const MAX_UNMAXIMIZED_WINDOW_AREA: f64 = 0.8;

        match self.home_of(window) {
            Some(Home::Floating) => (),
            // Only the floating half places a window where it could be too big for the screen.
            Some(Home::Scrolling) | Some(Home::Minimized) | None => return false,
        }
        let area = self.floating.working_area();

        let tile = self
            .floating
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let win = tile.window();
        let size = win.size();
        let min_size = win.min_size();
        let max_size = win.max_size();

        // mutter's has_maximize_func: skip windows that cannot cover the
        // work area.
        if (max_size.w > 0 && f64::from(max_size.w) < area.size.w)
            || (max_size.h > 0 && f64::from(max_size.h) < area.size.h)
        {
            return false;
        }

        let win_area = f64::from(size.w) * f64::from(size.h);
        if win_area <= area.size.w * area.size.h * MAX_UNMAXIMIZED_WINDOW_AREA {
            return false;
        }

        let factor = MAX_UNMAXIMIZED_WINDOW_AREA.sqrt();
        let scale = f64::min(
            area.size.w * factor / f64::from(size.w),
            area.size.h * factor / f64::from(size.h),
        )
        .min(1.);
        let mut restore = Size::from((
            (f64::from(size.w) * scale).round() as i32,
            (f64::from(size.h) * scale).round() as i32,
        ));
        restore.w = restore.w.max(min_size.w);
        restore.h = restore.h.max(min_size.h);

        self.set_maximized(window, true);

        // Clamp the restore size; maximizing stored the live near-work-area size as the rect to
        // come back to. In GNOME mode the tile stays put and that rect is `tiled_restore_size`; in
        // scrolling mode the tile moved to the scrolling layout and it is `floating_window_size`.
        if let Some(tile) = self
            .floating
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
        {
            tile.tiled_restore_size = Some(restore);
            tile.auto_maximized = true;
        } else if let Some(tile) = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
        {
            tile.floating_window_size = Some(restore);
            tile.auto_maximized = true;
        }
        true
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) {
        // GNOME mode: see `set_fullscreen`.
        if self.options.layout.windowing_mode == WindowingMode::Floating {
            self.floating.set_maximized(window, maximize);
            return;
        }

        let mut restore_to_floating = false;
        // The pre-tile rect of an edge-tiled window; it becomes the maximize
        // restore rect (mutter's saved_rect flows from tile to maximize).
        // Applied after the move to the scrolling layout, which stores the
        // live (tiled) geometry as the floating restore.
        let mut tiled_restore = None;
        // As in `set_fullscreen`: a chain, so the third state is a guard rather than an arm.
        match self.home_of(window) {
            Some(Home::Floating) | Some(Home::Scrolling) => (),
            Some(Home::Minimized) | None => return,
        }
        if self.floating.has_window(window) {
            if maximize {
                tiled_restore = self.floating.take_tile_restore(window);
                restore_to_floating = true;
                self.toggle_window_floating(Some(window));
            } else {
                // Floating windows are never maximized; but an edge-tiled one
                // counts as maximized for mutter's handle_unmaximize, which
                // untiles it.
                self.floating.untile_window(window);
                return;
            }
        } else if !maximize {
            // The window is in the scrolling layout and we're requesting to unmaximize. If it is
            // indeed maximized (i.e. this isn't a duplicate unmaximize request), then we may
            // need to unmaximize into floating.
            let tile = self
                .scrolling
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();
            // The tile cannot unmaximize into fullscreen (pending_sizing_mode() will be fullscreen
            // in that case and not maximized), so this check works.
            if tile.window().pending_sizing_mode().is_maximized() && tile.restore_to_floating {
                // Unmaximize and float in one call so it has a chance to notice and request a
                // (0, 0) size, rather than the scrolling column size.
                self.toggle_window_floating(Some(window));
                return;
            }
        }

        let tile = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.scrolling.set_maximized(window, maximize);

        // When going from normal to maximized, remember if we should unmaximize to floating.
        let tile = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && !tile.window().pending_sizing_mode().is_normal() {
            tile.restore_to_floating = restore_to_floating;
            if let Some((size, pos)) = tiled_restore {
                if size.is_some() {
                    tile.floating_window_size = size;
                }
                if pos.is_some() {
                    tile.floating_pos = pos;
                }
            }
        }
    }

    pub fn toggle_maximized(&mut self, window: &W::Id) {
        // We have to check the column property in case the window is in the scrolling layout and
        // both maximized and fullscreen. In this case, only the column knows whether it's
        // maximized. The floating layout keeps the same bit per tile (`saved_maximize`).
        let current = match self.scrolling.columns().find(|col| col.contains(window)) {
            Some(col) => col.is_pending_maximized(),
            None => self.floating.is_maximized(window),
        };

        self.set_maximized(window, !current);
    }

    pub fn toggle_window_floating(&mut self, id: Option<&W::Id>) {
        // GNOME mode has only the floating layout, so there is nothing to toggle to. This is the
        // one gate for every entry point — the `<Super>G` bind, the pointer and touch move grabs,
        // and the IPC action — all of which would otherwise put a window into a scrolling column
        // and bring back the side-by-side placement that mode exists to be rid of.
        if self.options.layout.windowing_mode == WindowingMode::Floating {
            return;
        }

        let active_id = self.active_window().map(|win| win.id().clone());
        let target_is_active = id.is_none_or(|id| Some(id) == active_id.as_ref());
        let Some(id) = id.cloned().or(active_id) else {
            return;
        };

        match self.home_of(&id) {
            Some(Home::Floating) | Some(Home::Scrolling) => (),
            // A hidden window is in neither half; it takes its half back on unminimize.
            Some(Home::Minimized) | None => return,
        }

        let (_, render_pos, _) = self
            .tiles_with_render_positions()
            .find(|(tile, _, _)| *tile.window().id() == id)
            .unwrap();

        if self.floating.has_window(&id) {
            let removed = self.floating.remove_tile(&id);
            // FIXME: compute closest pos?
            self.scrolling.add_tile(
                None,
                removed.tile,
                target_is_active,
                removed.width,
                removed.is_full_width,
                None,
            );
            if target_is_active {
                self.floating_is_active = FloatingActive::No;
            }
        } else {
            let mut removed = self.scrolling.remove_tile(&id, Transaction::new());
            removed.tile.stop_move_animations();

            // Come up with a default floating position close to the tile position.
            let stored_or_default = self.floating.stored_or_default_tile_pos(&removed.tile);
            if stored_or_default.is_none() {
                let offset =
                    if self.options.layout.center_focused_column == CenterFocusedColumn::Always {
                        Point::from((0., 0.))
                    } else {
                        Point::from((50., 50.))
                    };
                let pos = render_pos + offset;
                let size = removed.tile.tile_size();
                let pos = self.floating.clamp_within_working_area(pos, size);
                let pos = self.floating.logical_to_size_frac(pos);
                removed.tile.floating_pos = Some(pos);
            }

            self.floating.add_tile(removed.tile, target_is_active);
            if target_is_active {
                self.floating_is_active = FloatingActive::Yes;
            }
        }

        let (tile, new_render_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| *tile.window().id() == id)
            .unwrap();

        tile.animate_move_from(render_pos - new_render_pos);
    }

    pub fn set_window_floating(&mut self, id: Option<&W::Id>, floating: bool) {
        let is_floating = match self.target_home(id) {
            Some(Home::Floating) => true,
            Some(Home::Scrolling) => false,
            // A hidden window is in neither half; it takes its half back on unminimize.
            Some(Home::Minimized) | None => return,
        };
        if is_floating == floating {
            return;
        }

        self.toggle_window_floating(id);
    }

    pub fn focus_floating(&mut self) {
        if !self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn focus_tiling(&mut self) {
        if self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn switch_focus_floating_tiling(&mut self) {
        if self.floating.is_empty() {
            // If floating is empty, keep focus on scrolling.
            return;
        } else if self.scrolling.is_empty() {
            // If floating isn't empty but scrolling is, keep focus on floating.
            return;
        }

        self.floating_is_active = if self.floating_is_active.get() {
            FloatingActive::No
        } else {
            FloatingActive::Yes
        };
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        match self.target_home(id) {
            Some(Home::Floating) => self.floating.move_window(id, x, y, animate),
            Some(Home::Minimized) | None => (),
            Some(Home::Scrolling) => {
                // If the target tile isn't floating, set its stored floating position.
                let tile = if let Some(id) = id {
                    self.scrolling
                        .tiles_mut()
                        .find(|tile| tile.window().id() == id)
                        .unwrap()
                } else if let Some(tile) = self.scrolling.active_tile_mut() {
                    tile
                } else {
                    return;
                };

                let pos = self.floating.stored_or_default_tile_pos(tile);

                // If there's no stored floating position, we can only set both components at once,
                // not adjust.
                let pos = pos.or_else(|| {
                    (matches!(
                        x,
                        PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                    ) && matches!(
                        y,
                        PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                    ))
                    .then_some(Point::default())
                });

                let Some(mut pos) = pos else {
                    return;
                };

                let working_area = self.floating.working_area();
                let available_width = working_area.size.w;
                let available_height = working_area.size.h;
                let working_area_loc = working_area.loc;

                const MAX_F: f64 = 10000.;

                match x {
                    PositionChange::SetFixed(x) => pos.x = x + working_area_loc.x,
                    PositionChange::SetProportion(prop) => {
                        let prop = (prop / 100.).clamp(0., MAX_F);
                        pos.x = available_width * prop + working_area_loc.x;
                    }
                    PositionChange::AdjustFixed(x) => pos.x += x,
                    PositionChange::AdjustProportion(prop) => {
                        let current_prop = (pos.x - working_area_loc.x) / available_width.max(1.);
                        let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                        pos.x = available_width * prop + working_area_loc.x;
                    }
                }
                match y {
                    PositionChange::SetFixed(y) => pos.y = y + working_area_loc.y,
                    PositionChange::SetProportion(prop) => {
                        let prop = (prop / 100.).clamp(0., MAX_F);
                        pos.y = available_height * prop + working_area_loc.y;
                    }
                    PositionChange::AdjustFixed(y) => pos.y += y,
                    PositionChange::AdjustProportion(prop) => {
                        let current_prop = (pos.y - working_area_loc.y) / available_height.max(1.);
                        let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                        pos.y = available_height * prop + working_area_loc.y;
                    }
                }

                let pos = self.floating.logical_to_size_frac(pos);
                tile.floating_pos = Some(pos);
            }
        }
    }

    pub fn has_windows(&self) -> bool {
        self.windows().next().is_some()
    }

    /// Where `id` is, or `None` when this workspace does not hold it at all.
    fn home_of(&self, id: &W::Id) -> Option<Home> {
        if self.floating.has_window(id) {
            Some(Home::Floating)
        } else if self.scrolling.tiles().any(|tile| tile.window().id() == id) {
            Some(Home::Scrolling)
        } else if self.is_minimized(id) {
            Some(Home::Minimized)
        } else {
            None
        }
    }

    /// Where an operation aimed at `id` should go: the window's own home, or the active half
    /// when no window is named — `None` means "whatever is active", which is the convention the
    /// callers taking `Option<&W::Id>` use.
    fn target_home(&self, id: Option<&W::Id>) -> Option<Home> {
        match id {
            Some(id) => self.home_of(id),
            None => Some(if self.floating_is_active.get() {
                Home::Floating
            } else {
                Home::Scrolling
            }),
        }
    }

    /// Whether `window` is **laid out** on this workspace — in the scrolling or floating half,
    /// with geometry.
    ///
    /// Deliberately **false** for a minimized window, which is on the workspace but in neither
    /// half. `Layout` finds the workspace to operate on with this at some sixty call sites, and
    /// every one of them routes into an `if floating { … } else { … }` whose else-branch would
    /// then ask the scrolling half about a tile it does not have — `ScrollingSpace`'s lookups
    /// unwrap, so that is a panic, not a no-op. An operation on a hidden window doing nothing is
    /// the right answer anyway, and this is how it gets it without sixty guards.
    ///
    /// For "is this window on this workspace at all", use [`Self::holds_window`].
    pub fn has_window(&self, window: &W::Id) -> bool {
        let scrolling = self.scrolling.tiles();
        let floating = self.floating.tiles();
        scrolling
            .chain(floating)
            .any(|tile| tile.window().id() == window)
    }

    /// Whether the workspace holds `window` at all, minimized included — what "which workspace is
    /// this window on" means. See [`Self::has_window`] for why the two differ.
    pub fn holds_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn find_wl_surface(&self, wl_surface: &WlSurface) -> Option<&W> {
        self.windows().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn find_wl_surface_mut(&mut self, wl_surface: &WlSurface) -> Option<&mut W> {
        self.windows_mut().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let scrolling = self.scrolling.tiles_with_render_positions();

        let floating = self.floating.tiles_with_render_positions();
        let visible = self.is_floating_visible();
        let floating = floating.map(move |(tile, pos)| (tile, pos, visible));

        // Front-to-back, consistent with the render order.
        if self.scrolling_renders_on_top() {
            Box::new(scrolling.chain(floating)) as Box<dyn Iterator<Item = _>>
        } else {
            Box::new(floating.chain(scrolling))
        }
    }

    /// Each tile with the position the picker lays its slots out over: where it sits with
    /// nothing animating, **unrounded**. Same order as
    /// [`Self::tiles_with_render_positions`].
    ///
    /// The scrolling layer's view position rides in here and animates on its own account,
    /// but the picker is GNOME-mode-only and GNOME mode keeps every window in the floating
    /// layout, so there is nothing in that layer to lay out.
    fn tiles_with_settled_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> {
        let scrolling = self.scrolling.tiles_with_settled_positions();
        let floating = self.floating.tiles_with_offsets();
        if self.scrolling_renders_on_top() {
            Box::new(scrolling.chain(floating)) as Box<dyn Iterator<Item = _>>
        } else {
            Box::new(floating.chain(scrolling))
        }
    }

    /// GNOME windowing: the focused window is effectively topmost (mutter
    /// raises on click and on activation), so when the active window lives
    /// in the scrolling layer (a maximized or fullscreen window), that layer
    /// covers the floating one. Focus-follows-mouse activation
    /// ([`FloatingActive::NoButRaised`]) deliberately keeps floating on top.
    pub fn scrolling_renders_on_top(&self) -> bool {
        self.options.layout.windowing_mode == WindowingMode::Floating
            && matches!(self.floating_is_active, FloatingActive::No)
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scrolling = self.scrolling.tiles_with_render_positions_mut(round);
        let floating = self.floating.tiles_with_render_positions_mut(round);
        floating.chain(scrolling)
    }

    /// Every tile with what IPC reports about its geometry — **including the minimized ones**,
    /// which is what `Layout::with_windows` sweeps and therefore what IPC, the introspect API,
    /// the app system and the foreign-toplevel refresh all see. A hidden window is still a window
    /// those consumers must be able to name; it is only its *geometry* that is gone.
    ///
    /// A minimized tile gets the position-less template, the same one an interactively-moved
    /// window gets: it has no place on screen to report.
    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scrolling = self.scrolling.tiles_with_ipc_layouts();
        let floating = self.floating.tiles_with_ipc_layouts();
        let minimized = self.minimized.iter().map(|parked| {
            (
                &parked.removed.tile,
                parked.removed.tile.ipc_layout_template(),
            )
        });
        floating.chain(scrolling).chain(minimized)
    }

    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.floating_is_active.get() {
            self.floating.active_window_visual_rectangle()
        } else {
            self.scrolling.active_window_visual_rectangle()
        }
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        match self.home_of(window) {
            Some(Home::Floating) => self.floating.popup_target_rect(window),
            Some(Home::Scrolling) => self.scrolling.popup_target_rect(window),
            // A hidden window has no rect to unconstrain a popup against.
            Some(Home::Minimized) | None => None,
        }
    }

    pub fn render_scrolling(
        &self,
        ctx: RenderCtx,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement),
    ) {
        let scrolling_focus_ring = focus_ring && !self.floating_is_active();
        self.scrolling
            .render(ctx, scrolling_focus_ring, &mut |elem| push(elem.into()));
    }

    pub fn render_floating(
        &self,
        ctx: RenderCtx,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement),
    ) {
        if !self.is_floating_visible() {
            return;
        }

        let view_rect = Rectangle::from_size(self.view_size);
        let floating_focus_ring = focus_ring && self.floating_is_active();
        self.floating
            .render(ctx, view_rect, floating_focus_ring, &mut |elem| {
                push(elem.into())
            });
    }

    /// Windows on their way out: each minimizing tile drawn shrinking into its destination.
    ///
    /// gnome-shell eases the actor's scale, position *and* opacity together
    /// (`_minimizeWindow`, `windowManager.js:1198-1208`). This is the geometry half; the opacity
    /// half rides the tile's own alpha animation, started alongside in [`Self::minimize`].
    pub fn render_minimizing(
        &self,
        mut ctx: RenderCtx,
        push: &mut dyn FnMut(WorkspaceRenderElement),
    ) {
        let scale = self.scale().fractional_scale();
        for parked in &self.minimized {
            let Some(rect) = parked.shrinking_rect() else {
                continue;
            };
            let tile = &parked.removed.tile;
            let size = tile.tile_size();
            if size.w <= 0. {
                continue;
            }
            let tile_scale = rect.size.w / size.w;
            tile.render(ctx.r(), rect.loc, false, &mut |elem| {
                push(
                    RescaleRenderElement::from_element(
                        elem,
                        rect.loc.to_physical_precise_round(scale),
                        tile_scale,
                    )
                    .into(),
                )
            });
        }
    }

    /// The GNOME overview window picker ("exposé"): every tile with its
    /// current rect and its picker slot, both in workspace coordinates.
    ///
    /// Slots come from gnome-shell's layout strategy (see [`expose`]), over
    /// the working area, front-to-back like
    /// [`Self::tiles_with_render_positions`].
    /// The settled position the picker lays out over, aligned to physical pixels.
    ///
    /// Rounded exactly once, from a value that does not move while an animation runs.
    /// Shared with [`Self::expose_settled_pos`] so the value a test can observe cannot
    /// drift from the value the layout consumes.
    fn settled_pos(scale: f64, settled: Point<f64, Logical>) -> Point<f64, Logical> {
        settled.to_physical_precise_round(scale).to_logical(scale)
    }

    /// Every minimized tile with the rect the picker lays it out over.
    ///
    /// A parked window is in the picker: gnome-shell's `_isOverviewWindow` is `!skip_taskbar`
    /// with no minimized check (`workspace.js:1332`), and its layout strategy reads the window's
    /// frame rect, which mutter keeps across a minimize. Ours is kept the same way and for the
    /// same reason unminimize is exact — [`FloatingSpace::remove_tile_by_idx`] stamps
    /// `floating_pos` and `floating_window_size` onto the tile on the way out, and
    /// [`FloatingSpace::stored_or_default_tile_pos`] reads the position back.
    ///
    /// So the rect is the one the tile *had*, and minimizing with the overview open leaves the
    /// picker's input unchanged: the grid does not shuffle under the user. The two can disagree
    /// only for a window parked mostly off-screen, where [`Data::recompute_logical_pos`] clamps
    /// the fraction and this does not.
    fn minimized_layout_inputs(&self) -> impl Iterator<Item = (&Tile<W>, Rectangle<f64, Logical>)> {
        let scale = self.scale().fractional_scale();
        let area = self.floating.working_area();
        self.minimized.iter().map(move |parked| {
            let tile = &parked.removed.tile;
            let size = tile.tile_size();
            let pos = self
                .floating
                .stored_or_default_tile_pos(tile)
                .unwrap_or_else(|| center_preferring_top_left_in_area(area, size));
            (tile, Rectangle::new(Self::settled_pos(scale, pos), size))
        })
    }

    /// Every minimized tile with the rect its preview grows *out of* on the overview leg.
    ///
    /// The destination it shrank into, so the preview comes from where the user watched the
    /// window go. This is deliberately **not** the layout input above: feeding a destination into
    /// the grid would re-sort it and shuffle every other preview, which is the property
    /// `minimizing_with_the_overview_open_leaves_the_other_previews_where_they_are` pins.
    ///
    /// Falls back to the layout input for a window that was never seen to go anywhere — see
    /// [`Parked::dest`].
    fn minimized_render_rects(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Rectangle<f64, Logical>, f64)> {
        iter::zip(&self.minimized, self.minimized_layout_inputs()).map(
            |(parked, (tile, settled))| {
                let natural = tile.tile_size();
                if natural.w <= 0. {
                    return (tile, settled, 1.);
                }
                let dest = parked.dest;
                // Natural size at the destination's *position*, shrunk by the destination's own
                // scale: the rect keeps the size the draw scale is derived from, and the fourth
                // field carries how small the preview starts.
                (
                    tile,
                    Rectangle::new(dest.loc, natural),
                    dest.size.w / natural.w,
                )
            },
        )
    }

    /// The layout input one window contributes, if it is here.
    fn expose_input(&self, window: &W::Id) -> Option<ExposeInput> {
        let scale = self.scale().fractional_scale();
        self.tiles_with_settled_positions()
            .find(|(tile, _)| tile.window().id() == window)
            .map(|(tile, settled)| {
                let rect = Rectangle::new(Self::settled_pos(scale, settled), tile.tile_size());
                (tile.window().stable_sequence(), rect)
            })
            .or_else(|| {
                self.minimized_layout_inputs()
                    .find(|(tile, _)| tile.window().id() == window)
                    .map(|(tile, rect)| (tile.window().stable_sequence(), rect))
            })
    }

    /// [`Self::settled_pos`] for one window, if it is here.
    pub(super) fn expose_settled_pos(&self, window: &W::Id) -> Option<Point<f64, Logical>> {
        self.expose_settled_rect(window).map(|rect| rect.loc)
    }

    /// Where the window actually sits on this desktop, at its own size — the rect the picker
    /// lays a slot out *over*, before any expose scaling. What a thumbnail draws a miniature of.
    pub(super) fn expose_settled_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        self.expose_input(window).map(|(_, rect)| rect)
    }

    /// The layout input list a decision is made over: one entry per window here, in stable
    /// creation order, plus the reservation a drag is holding.
    ///
    /// Stable order, not the front-to-back one the tiles come in. `compute_grid` sorts stably
    /// and breaks no ties, so windows whose centres tie exactly — which centred placement
    /// makes ordinary — are ordered by nothing but the input, and the stacking order changes
    /// on every raise. gnome-shell lays out `_sortedWindows`, held in `get_stable_sequence()`
    /// order (`workspace.js:811-817`), for exactly this reason: there, a restack recomputes to
    /// the identical assignment.
    ///
    /// The *live* list — what a [`CloseFreeze`] is taken from, never what it hands back.
    fn expose_live_inputs(&self) -> Vec<ExposeInput> {
        let scale = self.scale().fractional_scale();
        let mut inputs: Vec<ExposeInput> = self
            .tiles_with_settled_positions()
            .map(|(tile, settled)| {
                let rect = Rectangle::new(Self::settled_pos(scale, settled), tile.tile_size());
                (tile.window().stable_sequence(), rect)
            })
            .chain(
                self.minimized_layout_inputs()
                    .map(|(tile, rect)| (tile.window().stable_sequence(), rect)),
            )
            .collect();
        inputs.sort_by_key(|(seq, _)| *seq);

        // A window being dragged out of the picker has left the workspace, but it is still
        // laid out for: it keeps its place in the order and its slot stays vacant, so the
        // previews around it hold still instead of closing the gap it left. That is
        // gnome-shell's mechanism exactly — a preview drag reparents the actor and freezes
        // nothing, the window staying in `_sortedWindows` for the whole drag
        // (`windowPreview.js:643-670`), which is what keeps a slot reserved for it.
        //
        // A pickup reserves the input *before* the removal takes the tile away, so for that
        // stretch the window is both. The reservation is dropped while that lasts: laying it
        // out twice would decide a grid over a window that does not exist, and poison the
        // held inputs with it.
        if let Some(reserved) = self
            .expose_reserved
            .filter(|reserved| !inputs.iter().any(|(seq, _)| *seq == reserved.0))
        {
            let at = inputs.partition_point(|(seq, _)| *seq < reserved.0);
            inputs.insert(at, reserved);
        }

        inputs
    }

    fn expose_layout(&self) -> ExposeLayout<'_, W> {
        // The tile's *render* rect, which the overview open/close leg interpolates from. The
        // slots are laid out over the *settled* rect instead ([`Self::expose_live_inputs`]),
        // and the two differ by `Tile::render_offset()` — a move animation or an
        // interactive-move offset.
        //
        // gnome-shell's layout strategy reads `metaWindow.get_frame_rect()` (`workspace.js`
        // `_getWindowCenter`, `computeLayout`), never the actor's animated position, and it
        // has to: `compute_grid` assigns rows by `center().y` and columns by `center().x`, so
        // an animating rect re-sorts the grid for as long as the animation runs and the whole
        // picker shuffles and snaps back when it lands. A drop's move-back animation did
        // exactly that to every other preview.
        //
        // A minimized tile has no rect on screen, so it interpolates from the same rect it is
        // laid out over: the one it had. gnome-shell instead gives a window that is not
        // `showing_on_its_workspace` the work-area origin at **zero size**
        // (`workspace.js:709-720`) and fades it in over that. We cannot: this leg interpolates a
        // *scale*, `slot.size.w / rect.size.w` ([`expose_tile_render`]), so a zero-width `from`
        // divides by zero and the preview never draws at any progress — a slot with a hole in it.
        // Reaching zero size would mean an opacity ramp to hide the degenerate scale, which is
        // the fade this port does not have yet.
        let tiles: Vec<_> = self
            .tiles_with_render_positions()
            .map(|(tile, pos, _)| (tile, Rectangle::new(pos, tile.tile_size()), 1.))
            .chain(self.minimized_render_rects())
            .collect();

        let mut inputs = self.expose_live_inputs();

        // A freeze substitutes the whole list — see [`CloseFreeze`]. It subsumes the
        // reservation above, which it was taken with.
        if let Some(freeze) = &self.expose_freeze {
            debug_assert!(
                tiles.iter().all(|(tile, _, _)| freeze
                    .inputs
                    .iter()
                    .any(|(seq, _)| *seq == tile.window().stable_sequence())),
                "a tile arrived without releasing the freeze; it has no slot to land in",
            );
            inputs.clone_from(&freeze.inputs);
        }

        let packed = self.retained_expose_slots(inputs.clone(), self.view_size, self.expose_area());

        // Scatter the slots back onto the render order the caller expects, by identity: an
        // input no tile answers to — one carried by a drag, or held open by a freeze — simply
        // leaves its slot vacant.
        let mut slots = vec![Rectangle::default(); tiles.len()];
        for ((seq, _), slot) in iter::zip(&inputs, packed) {
            if let Some(i) = tiles
                .iter()
                .position(|(tile, _, _)| tile.window().stable_sequence() == *seq)
            {
                slots[i] = slot;
            }
        }

        tiles
            .into_iter()
            .zip(slots)
            .map(|((tile, rect, from_scale), slot)| {
                (tile, rect, self.slide_slot(tile, slot), from_scale)
            })
            .collect()
    }

    /// The picker's slots for `inputs`, deciding the layout only if this is not the
    /// decision it is already holding.
    ///
    /// The decision is validated by comparing the inputs it was reached from, not by a flag
    /// some mutator was supposed to set — see [`RetainedExpose::inputs`]. A hit re-packs the
    /// held grid, which reads window *sizes* only and so cannot re-order anything; a miss
    /// decides afresh and is indistinguishable from never having held one.
    fn retained_expose_slots(
        &self,
        inputs: Vec<ExposeInput>,
        view_size: Size<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Vec<Rectangle<f64, Logical>> {
        // An empty workspace has no layout to decide, and the overview draws several of
        // them: every workspace in the strip is rendered, and dynamic workspaces keep a
        // trailing empty one. Deciding nothing must not read as a decision.
        if inputs.is_empty() {
            *self.expose_retained.borrow_mut() = None;
            return Vec::new();
        }

        let mut retained = self.expose_retained.borrow_mut();

        let hit = retained.as_ref().is_some_and(|r| {
            (r.view_size.w.to_bits(), r.view_size.h.to_bits())
                == (view_size.w.to_bits(), view_size.h.to_bits())
                && r.inputs.len() == inputs.len()
                && iter::zip(&r.inputs, &inputs).all(|(a, b)| same_expose_input(a, b))
        });

        let rects: Vec<_> = inputs.iter().map(|(_, rect)| *rect).collect();
        if hit {
            // Packing reads window sizes only, so it cannot re-order anything: the same
            // decision, re-fitted. A changed area therefore moves and re-scales the previews
            // and never re-seats them, which is what makes a strut appearing mid-overview a
            // shift rather than a shuffle.
            let r = retained.as_mut().unwrap();
            return expose::pack_grid(&mut r.grid, area, &rects);
        }

        self.expose_recomputes.set(self.expose_recomputes.get() + 1);
        let mut grid = expose::compute_grid(view_size.h, area, &rects)
            .expect("a non-empty workspace always has a grid");
        let slots = expose::pack_grid(&mut grid, area, &rects);
        *retained = Some(RetainedExpose {
            inputs,
            view_size,
            grid,
        });
        slots
    }

    /// How many windows the picker's standing decision was made over.
    ///
    /// Larger than the tile count while a drag reserves a place for the window it carries,
    /// which is the only way to see that the reservation is still in force: a reserved slot
    /// has no tile asking for it and so never reaches a caller.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn expose_decided_over(&self) -> usize {
        self.expose_retained
            .borrow()
            .as_ref()
            .map_or(0, |r| r.inputs.len())
    }

    /// How many times this workspace has decided a picker layout — see
    /// [`Workspace::expose_recomputes`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn expose_recompute_count(&self) -> u64 {
        self.expose_recomputes.get()
    }

    /// A preview's slot on the way to the one just computed for it, if it is mid-ease.
    ///
    /// A **post-pass**, deliberately: the interpolated slot must never reach
    /// [`expose::compute_slots`], which sorts previews into rows and columns by their
    /// centres. An animating input re-sorts the grid every frame and the whole picker
    /// shuffles — the reason the layout above is taken over settled rects in the first
    /// place.
    fn slide_slot(&self, tile: &Tile<W>, slot: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        let Some((_, slide)) = self
            .expose_slides
            .iter()
            .find(|(id, _)| id == tile.window().id())
        else {
            return slot;
        };
        let t = slide.anim.clamped_value().clamp(0., 1.);
        let lerp = |a: f64, b: f64| a + (b - a) * t;
        Rectangle::new(
            Point::from((
                lerp(slide.from.loc.x, slot.loc.x),
                lerp(slide.from.loc.y, slot.loc.y),
            )),
            Size::from((
                lerp(slide.from.size.w, slot.size.w),
                lerp(slide.from.size.h, slot.size.h),
            )),
        )
    }

    /// The picker slots as drawn right now — the `from` of a slot ease. Taken *before*
    /// whatever changes the layout, and handed straight back to
    /// [`Self::slide_expose_slots_from`] after.
    pub(super) fn expose_slots_now(&self) -> Vec<(W::Id, Rectangle<f64, Logical>)> {
        self.expose_layout()
            .into_iter()
            .map(|(tile, _, slot, _)| (tile.window().id().clone(), slot))
            .collect()
    }

    /// Eases every preview from where it was to where the picker now puts it — gnome-shell
    /// easing each child from its current allocation on `layoutChanged`
    /// (`workspace.js:759-766`).
    ///
    /// `from` is a snapshot from [`Self::expose_slots_now`]; a window missing from it is
    /// one the layout has only just gained, and it comes in from `arriving` — the rect it
    /// was actually let go at, which is not derivable here.
    ///
    /// Taking the `from` as a snapshot rather than diffing against a remembered layout is
    /// what keeps this out of the render path: the caller knows when it changed the
    /// layout, and nothing has to notice after the fact.
    pub(super) fn slide_expose_slots_from(
        &mut self,
        from: Vec<(W::Id, Rectangle<f64, Logical>)>,
        arriving: Option<(W::Id, Rectangle<f64, Logical>)>,
    ) {
        if self.options.animations.off {
            return;
        }
        let config = synoik_config::Animation {
            off: false,
            kind: synoik_config::animations::Kind::Easing(
                synoik_config::animations::EasingParams {
                    duration_ms: SLOT_SLIDE_MS,
                    curve: synoik_config::animations::Curve::EaseOutQuad,
                },
            ),
        };

        // Cleared first so the snapshot below is the picker's own answer, not one already
        // being interpolated by a previous drop.
        self.expose_slides.clear();
        self.expose_slides = self
            .expose_slots_now()
            .into_iter()
            .filter_map(|(id, now)| {
                let was = from
                    .iter()
                    .chain(arriving.as_ref())
                    .find(|(other, _)| *other == id)
                    .map(|(_, rect)| *rect)?;
                // A preview the change did not move has nothing to ease, and arming one
                // anyway would keep the compositor drawing frames for 200ms of nothing.
                (was != now).then(|| {
                    let slide = SlotSlide {
                        from: was,
                        anim: Animation::new(self.clock.clone(), 0., 1., 0., config),
                    };
                    (id, slide)
                })
            })
            .collect();
    }

    /// The area the picker lays its slots out in: the working area, **symmetrized about the
    /// view** — each axis inset by the larger of that axis' two struts.
    ///
    /// **Divergence (approved 2026-07-28).** gnome-shell lays out over the raw work area
    /// (`_getAdjustedWorkarea`, `workspace.js:573-581`, minus the container's theme-node
    /// padding, which `.window-picker` doesn't set in 50.1). Its slots are therefore centered
    /// on the *work area* while the workspace background they sit on is the whole monitor, so
    /// the top panel's strut is clearance the top edge gets and the bottom edge does not: at
    /// 1920×1080 a maximized window's preview came out with 40px at the sides and 51 above it,
    /// but only 22 below — which reads as the window touching the bottom of the workspace.
    ///
    /// Insetting by the *larger* strut on each axis, rather than centering on the view
    /// outright, is what keeps this from putting a preview underneath a bottom dock: the area
    /// is always a subset of the working area, so every strut is still respected. Nothing is
    /// scaled down by it — a preview at the `MAXIMUM_SCALE` cap keeps its size and only moves.
    ///
    /// Note a padding constant here would have been a no-op: the cap already binds, so an
    /// inset has to exceed the slack under it (~26px at 1920×1080) before it moves anything.
    fn expose_area(&self) -> Rectangle<f64, Logical> {
        let work = self.floating.working_area();
        let view = Rectangle::from_size(self.view_size);

        let inset = |lo: f64, hi: f64| -> (f64, f64) {
            let strut = f64::max(lo, hi);
            (strut, strut * 2.)
        };
        let (x, dw) = inset(
            work.loc.x - view.loc.x,
            view.size.w - (work.loc.x + work.size.w),
        );
        let (y, dh) = inset(
            work.loc.y - view.loc.y,
            view.size.h - (work.loc.y + work.size.h),
        );

        Rectangle::new(
            Point::from((view.loc.x + x, view.loc.y + y)),
            Size::from((
                f64::max(view.size.w - dw, 1.),
                f64::max(view.size.h - dh, 1.),
            )),
        )
    }

    /// Point the picker overlay at `window` (or at nothing), easing the previous
    /// one back down — gnome-shell's enter/leave `showOverlay`/`hideOverlay`
    /// (`windowPreview.js:561-568`). A window that isn't on this workspace just
    /// clears it, so the caller can hand the same target to every workspace.
    /// Returns whether anything changed.
    ///
    /// `holds_window`, not `has_window`: a minimized window has a preview in the picker like any
    /// other, and asking the *laid out here* question left it the one preview that could not be
    /// hovered — no growth, and no close button, since both are gated on the hover being armed.
    pub(super) fn set_expose_hover(&mut self, window: Option<&W::Id>) -> bool {
        let mine = window.filter(|id| self.holds_window(id));

        let mut changed = false;
        for (id, anim) in &mut self.expose_hover {
            if Some(&*id) != mine && anim.to() != 0. {
                *anim = ease_hover(&self.clock, anim.value(), 0., &self.options);
                changed = true;
            }
        }

        if let Some(window) = mine {
            if let Some((_, anim)) = self.expose_hover.iter_mut().find(|(id, _)| id == window) {
                if anim.to() != 1. {
                    *anim = ease_hover(&self.clock, anim.value(), 1., &self.options);
                    changed = true;
                }
            } else {
                let anim = ease_hover(&self.clock, 0., 1., &self.options);
                self.expose_hover.push((window.clone(), anim));
                changed = true;
            }
        }

        changed
    }

    /// Drop the picker overlay outright, with no ease back down — for leaving the
    /// overview, where an eased hover would keep [`Self::render_expose`] restacking
    /// the preview above its neighbours for the whole exit animation.
    pub(super) fn clear_expose_hover(&mut self) {
        self.expose_hover.clear();
        // Leaving the picker: an eased slot would keep `render_expose` restacking (and
        // would be interpolating toward a layout nobody is looking at).
        self.expose_slides.clear();
    }

    /// Every window with an overlay showing, and how far it has faded in. The
    /// render path wants *all* previews (the app icon is not hover-gated) and goes
    /// through [`Self::expose_hover_value`] instead; this stays as the "is anything
    /// hovered at all" query.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn expose_hovers(&self) -> impl Iterator<Item = (&W::Id, f64)> + '_ {
        self.expose_hover.iter().filter_map(|(id, anim)| {
            let value = anim.clamped_value().clamp(0., 1.);
            (value > 0.).then_some((id, value))
        })
    }

    /// The picker-overlay progress of one window: 0 idle, 1 fully hovered.
    pub(super) fn expose_hover_value(&self, window: &W::Id) -> f64 {
        self.expose_hover
            .iter()
            .find(|(id, _)| id == window)
            .map_or(0., |(_, anim)| anim.clamped_value().clamp(0., 1.))
    }

    /// Keeps laying out for `window` until [`Self::unfreeze_expose`], though it has left
    /// this workspace to ride along with the drag.
    ///
    /// Membership is a layout input, so a window picked up would otherwise be a window
    /// removed, and the previews around it would close the gap it left. gnome-shell has
    /// nothing to suppress here: a preview drag reparents the actor and leaves the window in
    /// `_sortedWindows` (`windowPreview.js:643-670`), so the layout keeps computing a slot
    /// that simply has no actor in it. Holding the input back is that, in a layout that owns
    /// its tiles.
    ///
    /// It reserves the *input*, not the slot it resolved to. A slot captured at pickup would
    /// be captured through the slide post-pass and pinned mid-flight, so a drag begun during
    /// a settling drop froze every preview part-way to where it was going, for the length of
    /// the drag.
    pub(super) fn freeze_expose(&mut self, window: &W::Id) {
        self.expose_reserved = self.expose_input(window);
    }

    /// Whether a drag has already reserved this window's place in the picker, i.e. whether
    /// the removal about to happen is a pickup rather than a departure.
    /// Whether a drag is holding a reservation here at all.
    ///
    /// Not [`Self::expose_is_reserved`], which asks whether a *given* window's removal is the
    /// pickup that reserved it — and so needs the window to still be a tile. Once the drag is
    /// in flight the tile lives in the move state and no workspace can answer that.
    pub(super) fn expose_has_reservation(&self) -> bool {
        self.expose_reserved.is_some()
    }

    pub(super) fn expose_is_reserved(&self, window: &W::Id) -> bool {
        let reserved = self.expose_reserved.map(|(seq, _)| seq);
        reserved.is_some() && reserved == self.expose_input(window).map(|(seq, _)| seq)
    }

    /// Forget the held picker layout, so the next one is decided afresh.
    ///
    /// Called when the overview opens, which is what bounds the one staleness holding a
    /// decision permits: the packing area is not compared, so a strut that appears mid-visit
    /// re-fits the previews rather than re-seating them, and a re-fit can only shrink
    /// (`additional_scale` caps at 1). Deciding again at each entry keeps that to a single
    /// visit. It is also gnome-shell's own scope: entering the overview runs
    /// `_updateWorkspacesViews` (`workspacesView.js:998`), which rebuilds every `Workspace`,
    /// and a fresh `WorkspaceLayout` starts with `_needsLayout` set (`workspace.js:430`).
    pub(super) fn forget_expose_layout(&mut self) {
        *self.expose_retained.borrow_mut() = None;
        // A freeze taken during a peek, or left over from an exit, has nothing to say about
        // the visit starting now — and holding a stale input list into it would keep the
        // decision below from ever being made over what is actually here.
        self.expose_freeze = None;
    }

    pub(super) fn unfreeze_expose(&mut self) {
        self.expose_reserved = None;
    }

    /// Hold the picker's layout past a removal, so the previews do not reflow out from under
    /// a pointer that is still working in them — gnome-shell's `_doRemoveWindow`
    /// (`workspace.js:1140-1183`). Call it *before* the removal: the list it takes is the
    /// layout that is on screen.
    ///
    /// Takes no window, because a freeze is not about one: it holds the whole assignment,
    /// which is what `layout_frozen` does to an allocation.
    pub(super) fn freeze_expose_for_close(&mut self) {
        let hold = self.fresh_close_hold();

        match &mut self.expose_freeze {
            // A second removal inside the window shares the hold and only re-arms it, which
            // is gnome-shell dropping and re-adding the one timeout at `workspace.js:1154-1161`.
            // The list already held describes what is on screen; a fresh one would describe
            // the layout as of *after* the previous removal, which nobody has seen.
            Some(freeze) => freeze.hold = hold,
            None => {
                let inputs = self.expose_live_inputs();
                self.expose_freeze = Some(CloseFreeze { inputs, hold });
            }
        }
    }

    /// Let the picker reflow, easing every preview from the slot the freeze was holding it
    /// at into the one it has now — gnome-shell clearing `layout_frozen` (`workspace.js:937`)
    /// onto the `_needsLayout` the removal set, which allocates through `animateAllocation`.
    fn release_expose_freeze(&mut self) {
        if self.expose_freeze.is_none() {
            return;
        }
        let before = self.expose_slots_now();
        self.expose_freeze = None;
        self.slide_expose_slots_from(before, None);
    }

    /// A window arriving releases the freeze — gnome-shell's `_doAddWindow`, "to ensure the
    /// new window is immediately shown" (`workspace.js:1245-1251`).
    ///
    /// Also an **invariant**, not just fidelity: a freeze holds a fixed input list, and a tile
    /// with no entry in it would scatter to nowhere and draw at the origin. Every insertion
    /// funnels through here for that reason.
    ///
    /// Per workspace, like gnome-shell's: an arrival on one workspace has no bearing on a
    /// removal still settling on another.
    fn release_expose_freeze_on_arrival(&mut self) {
        self.release_expose_freeze();
    }

    /// Stop a hold from running out, without ending it — for the overview exit.
    ///
    /// The exit interpolates each preview between its window rect and its slot, so a hold that
    /// expired partway through would reflow the slots mid-flight and shuffle the picker on its
    /// way out. gnome-shell removes the timeout and *sets* `layout_frozen` at exit start
    /// (`prepareToLeaveOverview`, `workspace.js:1295-1303`) for exactly this; the freeze is
    /// dropped outright once hidden.
    pub(super) fn hold_expose_freeze_through_exit(&mut self) {
        if let Some(freeze) = &mut self.expose_freeze {
            freeze.hold = None;
        }
    }

    /// Drop a freeze without easing anything, for when nobody is looking at the picker.
    pub(super) fn forget_expose_freeze(&mut self) {
        self.expose_freeze = None;
    }

    /// Whether the pointer is on a preview of a window that is *still here*.
    ///
    /// Still here matters: closing a window by its own close button leaves the pointer over a
    /// preview that no longer exists, and nothing clears that hover entry until the next
    /// motion. Counting the corpse would convert the hold to the indefinite kind and freeze
    /// the picker until the user moved the mouse.
    fn expose_hovers_a_live_preview(&self) -> bool {
        self.expose_hover
            .iter()
            .any(|(id, anim)| anim.to() == 1. && self.holds_window(id))
    }

    /// The pointer moved; `over_this_workspace` is whether it is inside this workspace's
    /// picker. Returns whether the freeze state changed.
    ///
    /// gnome-shell samples the pointer on a 750ms tick and continues the hold when it has
    /// *moved and is over this workspace*, or is over one of its previews at all
    /// (`workspace.js:1164-1174`). We are handed the motion instead of polling for it, which
    /// is strictly sharper: a pointer that wanders away and back to the same pixel reads as
    /// moving here and as still there.
    pub(super) fn expose_pointer_moved(&mut self, over_this_workspace: bool) -> bool {
        if self.expose_freeze.is_none() {
            return false;
        }
        let on_preview = self.expose_hovers_a_live_preview();
        let hold = self.fresh_close_hold();
        let freeze = self.expose_freeze.as_mut().unwrap();

        if on_preview {
            let changed = freeze.hold.is_some();
            freeze.hold = None;
            return changed;
        }

        // Re-arming an indefinite hold here is what keeps it from being permanent: without
        // it, a pointer that leaves a preview for another output — or the panel, or a
        // thumbnail — is never seen again and the picker stays frozen for good. gnome-shell
        // cannot reach that state because its tick re-evaluates the whole predicate every
        // time, whether or not anything moved.
        if over_this_workspace || freeze.hold.is_none() {
            freeze.hold = hold;
            return true;
        }

        false
    }

    /// A hold that runs out [`CLOSE_FREEZE_MS`] from now.
    fn fresh_close_hold(&self) -> Option<Animation> {
        Some(Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            synoik_config::Animation {
                off: self.options.animations.overview_open_close.0.off,
                kind: synoik_config::animations::Kind::Easing(
                    synoik_config::animations::EasingParams {
                        duration_ms: CLOSE_FREEZE_MS,
                        curve: synoik_config::animations::Curve::Linear,
                    },
                ),
            },
        ))
    }

    /// The picker slot of one window, in workspace coordinates.
    pub(super) fn expose_slot(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        self.expose_layout()
            .into_iter()
            .find(|(tile, _, _, _)| tile.window().id() == window)
            .map(|(_, _, slot, _)| slot)
    }

    /// Renders the workspace as the GNOME overview window picker: each tile
    /// at its slot, interpolated from its real rect by `progress`.
    pub fn render_expose(
        &self,
        mut ctx: RenderCtx,
        progress: f64,
        zoom: f64,
        push: &mut dyn FnMut(WorkspaceRenderElement),
    ) {
        let scale = self.scale().fractional_scale();

        // The hovered preview draws on top of its neighbours (`_restack`,
        // `windowPreview.js:620`); first pushed is topmost.
        let mut layout = self.expose_layout();
        if let Some(i) = layout
            .iter()
            .position(|(tile, _, _, _)| self.expose_hover_value(tile.window().id()) > 0.)
        {
            let hovered = layout.remove(i);
            layout.insert(0, hovered);
        }

        for (tile, rect, slot, from_scale) in layout {
            let hover = self.expose_hover_value(tile.window().id());
            let (pos, tile_scale) =
                expose_tile_placement(rect, slot, from_scale, hover, progress, zoom, scale);

            tile.render(ctx.r(), pos, false, &mut |elem| {
                push(
                    RescaleRenderElement::from_element(
                        elem,
                        pos.to_physical_precise_round(scale),
                        tile_scale,
                    )
                    .into(),
                )
            });
        }
    }

    /// The rect a preview draws into at expose `progress`, in workspace
    /// coordinates — [`Self::expose_slot`] plus the hover growth. Same geometry
    /// [`Self::render_expose`] draws.
    pub(super) fn expose_drawn_rect(
        &self,
        window: &W::Id,
        progress: f64,
        zoom: f64,
    ) -> Option<Rectangle<f64, Logical>> {
        let (_, rect, slot, from_scale) = self
            .expose_layout()
            .into_iter()
            .find(|(tile, _, _, _)| tile.window().id() == window)?;
        let hover = self.expose_hover_value(window);
        let (pos, tile_scale) = expose_tile_render(rect, slot, from_scale, hover, progress, zoom);
        Some(Rectangle::new(pos, rect.size.upscale(tile_scale)))
    }

    /// Hit test for the exposé picker: slots, front-to-back. Activation hits
    /// only — real input can't be routed to a scaled window.
    pub(super) fn window_under_expose(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        self.expose_layout()
            .into_iter()
            .find_map(|(tile, _, slot, _)| {
                slot.contains(pos).then(|| {
                    (
                        tile.window(),
                        HitType::Activate {
                            is_tab_indicator: false,
                        },
                    )
                })
            })
    }

    pub fn render_shadow(&self, push: &mut dyn FnMut(ShadowRenderElement)) {
        self.shadow.render(Point::from((0., 0.)), push);
    }

    pub fn render_background(&self) -> SolidColorRenderElement {
        SolidColorRenderElement::from_buffer(
            &self.background_buffer,
            Point::new(0., 0.),
            1.,
            Kind::Unspecified,
        )
    }

    pub fn render_above_top_layer(&self) -> bool {
        // GNOME mode keeps fullscreen windows in the floating layout, so that is where to ask.
        if self.options.layout.windowing_mode == WindowingMode::Floating {
            return self.floating.render_above_top_layer();
        }

        self.scrolling.render_above_top_layer()
    }

    pub fn is_floating_visible(&self) -> bool {
        // If the focus is on a fullscreen scrolling window, hide the floating windows.
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        ) || !self.render_above_top_layer()
    }

    pub fn store_unmap_snapshot_if_empty(&mut self, renderer: SnapshotRenderer, window: &W::Id) {
        let view_size = self.view_size();
        for (tile, tile_pos) in self.tiles_with_render_positions_mut(false) {
            if tile.window().id() == window {
                let view_pos = Point::from((-tile_pos.x, -tile_pos.y));
                let view_rect = Rectangle::new(view_pos, view_size);
                tile.update_render_elements(false, view_rect);
                tile.store_unmap_snapshot_if_empty(renderer);
                return;
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        for tile in self.tiles_mut() {
            if tile.window().id() == window {
                let _ = tile.take_unmap_snapshot();
                return;
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        match self.home_of(window) {
            Some(Home::Floating) => self
                .floating
                .start_close_animation_for_window(window, blocker),
            Some(Home::Scrolling) => self
                .scrolling
                .start_close_animation_for_window(window, blocker),
            // Nothing on screen to animate away.
            Some(Home::Minimized) | None => (),
        }
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        snapshot: TileUnmapSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        self.floating
            .start_close_animation_for_tile(snapshot, tile_size, tile_pos, blocker);
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.scrolling.start_open_animation(id) || self.floating.start_open_animation(id)
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // This logic is consistent with tiles_with_render_positions().
        if self.scrolling_renders_on_top() {
            if let Some(rv) = self.scrolling.window_under(pos) {
                return Some(rv);
            }
        }

        if self.is_floating_visible() {
            if let Some(rv) = self
                .floating
                .tiles_with_render_positions()
                .find_map(|(tile, tile_pos)| HitType::hit_tile(tile, tile_pos, pos))
            {
                return Some(rv);
            }
        }

        if self.scrolling_renders_on_top() {
            return None;
        }
        self.scrolling.window_under(pos)
    }

    pub fn resize_edges_under(&self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.tiles_with_render_positions()
            .find_map(|(tile, tile_pos, visible)| {
                // This logic should be consistent with window_under() in when it returns Some vs.
                // None.
                if !visible {
                    return None;
                }

                let pos_within_tile = pos - tile_pos;

                if tile.hit(pos_within_tile).is_some() {
                    let size = tile.tile_size().to_f64();

                    let mut edges = ResizeEdge::empty();
                    if pos_within_tile.x < size.w / 3. {
                        edges |= ResizeEdge::LEFT;
                    } else if 2. * size.w / 3. < pos_within_tile.x {
                        edges |= ResizeEdge::RIGHT;
                    }
                    if pos_within_tile.y < size.h / 3. {
                        edges |= ResizeEdge::TOP;
                    } else if 2. * size.h / 3. < pos_within_tile.y {
                        edges |= ResizeEdge::BOTTOM;
                    }
                    return Some(edges);
                }

                None
            })
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.floating.descendants_added(id)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if !self.floating.update_window(window, serial) {
            self.scrolling.update_window(window, serial);
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        self.scrolling
            .refresh(is_active && !self.floating_is_active.get(), is_focused);
        self.floating
            .refresh(is_active && self.floating_is_active.get(), is_focused);
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        match self.home_of(window) {
            Some(Home::Scrolling) => self.scrolling.scroll_amount_to_activate(window),
            // Nothing to scroll to: a floating window is already in view, and a minimized one
            // is not in the view at all.
            Some(Home::Floating) | Some(Home::Minimized) | None => 0.,
        }
    }

    pub fn is_urgent(&self) -> bool {
        self.windows().any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        // Activating unminimizes on the way in — `meta_window_activate_full`'s
        // `meta_window_unminimize` (`window.c:3908`). Hooked here rather than in `Layout` so
        // every path that raises a window (the dash, the switcher, an activation token, a click
        // in the overview) brings a minimized one back instead of silently doing nothing.
        self.unminimize(window, ActivateWindow::Yes);

        if self.floating.activate_window(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            true
        } else {
            false
        }
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) -> bool {
        // Same unminimize, but this caller is explicitly not raising, so the window comes back
        // without taking activation with it.
        self.unminimize(window, ActivateWindow::No);

        if self.floating.activate_window_without_raising(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No => FloatingActive::No,
                FloatingActive::NoButRaised => FloatingActive::NoButRaised,
                FloatingActive::Yes => FloatingActive::NoButRaised,
            };
            true
        } else {
            false
        }
    }

    pub(super) fn scrolling_insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        self.scrolling.insert_position(pos)
    }

    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        if let InsertPosition::EdgeTile(target) = position {
            return Some(self.edge_tile_area(target));
        }
        self.scrolling.insert_hint_area(position)
    }

    /// The edge zone under `pos`, if dropping a dragged window there should
    /// tile or maximize it.
    ///
    /// mutter `meta-window-drag.c`, `update_move_maybe_tile`: a band of
    /// `shake_threshold` px (drag threshold 8 × 6 = 48) at the left/right
    /// work-area edge tiles to that half; the strip between the monitor's top
    /// edge and the work area's top (deliberately the *outside* edge, so
    /// windows can still be placed near the top) maximizes.
    pub(super) fn edge_tile_target(&self, pos: Point<f64, Logical>) -> Option<EdgeTileTarget> {
        if self.options.layout.windowing_mode != WindowingMode::Floating {
            return None;
        }

        use crate::gnome::SHAKE_THRESHOLD;
        let area = self.floating.working_area();
        if pos.x < area.loc.x + SHAKE_THRESHOLD {
            Some(EdgeTileTarget::Tile(TileSide::Left))
        } else if pos.x >= area.loc.x + area.size.w - SHAKE_THRESHOLD {
            Some(EdgeTileTarget::Tile(TileSide::Right))
        } else if pos.y <= area.loc.y {
            Some(EdgeTileTarget::Maximize)
        } else {
            None
        }
    }

    /// The area an edge drop would give the window (mutter's
    /// `meta_window_get_tile_area`): half the work area for the sides, all of
    /// it for maximize.
    fn edge_tile_area(&self, target: EdgeTileTarget) -> Rectangle<f64, Logical> {
        let area = self.floating.working_area();
        match target {
            EdgeTileTarget::Tile(side) => {
                let width = (area.size.w / 2.).round();
                let x = match side {
                    TileSide::Left => area.loc.x,
                    TileSide::Right => area.loc.x + area.size.w - width,
                };
                Rectangle::new(
                    Point::from((x, area.loc.y)),
                    Size::from((width, area.size.h)),
                )
            }
            EdgeTileTarget::Maximize => area,
        }
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        self.scrolling.view_offset_gesture_begin(is_touchpad);
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.scrolling
            .view_offset_gesture_update(delta_x, timestamp, is_touchpad)
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        self.scrolling.view_offset_gesture_end(is_touchpad)
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        self.scrolling.dnd_scroll_gesture_begin();
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
        let config = &self.options.gestures.dnd_edge_view_scroll;
        let trigger_width = config.trigger_width;

        // This working area intentionally does not include extra struts from Options.
        let x = pos.x - self.working_area.loc.x;
        let width = self.working_area.size.w;

        let x = x.clamp(0., width);
        let trigger_width = trigger_width.clamp(0., width / 2.);

        let delta = if x < trigger_width {
            -(trigger_width - x)
        } else if width - x < trigger_width {
            trigger_width - (width - x)
        } else {
            0.
        };

        let delta = if trigger_width < 0.01 {
            // Sanity check for trigger-width 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_width
        };
        let delta = delta * speed;

        self.scrolling.dnd_scroll_gesture_scroll(delta)
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        self.scrolling.dnd_scroll_gesture_end();
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        match self.home_of(&window) {
            Some(Home::Floating) => self.floating.interactive_resize_begin(window, edges),
            Some(Home::Scrolling) => self.scrolling.interactive_resize_begin(window, edges),
            // Nothing to grab the edge of.
            Some(Home::Minimized) | None => false,
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        match self.home_of(window) {
            Some(Home::Floating) => self.floating.interactive_resize_update(window, delta),
            Some(Home::Scrolling) => self.scrolling.interactive_resize_update(window, delta),
            Some(Home::Minimized) | None => false,
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            match self.home_of(window) {
                Some(Home::Floating) => self.floating.interactive_resize_end(Some(window)),
                Some(Home::Scrolling) => self.scrolling.interactive_resize_end(Some(window)),
                Some(Home::Minimized) | None => (),
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.scrolling.interactive_resize_end(None);
        }
    }

    pub fn floating_is_active(&self) -> bool {
        self.floating_is_active.get()
    }

    pub fn floating_logical_to_size_frac(
        &self,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.floating.logical_to_size_frac(logical_pos)
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    /// Seeds the geometry an un-maximize or un-fullscreen returns `id` to, from an output-local
    /// rect.
    ///
    /// A window that maps straight into maximized or fullscreen never had a floating incarnation
    /// this run, so nothing has filled in `tiled_restore_*` and `restore_normal` would fall back
    /// to a default size. Session restore is the one caller that knows better, because it kept the
    /// rect from last time.
    ///
    /// Returns whether the window was found here.
    pub fn seed_unmaximize_geometry(&mut self, id: &W::Id, rect: Rectangle<f64, Logical>) -> bool {
        let pos = self.floating.logical_to_size_frac(rect.loc);

        let Some(tile) = self.tiles_mut().find(|tile| tile.window().id() == id) else {
            return false;
        };

        tile.tiled_restore_size = Some(Size::from((
            rect.size.w.round() as i32,
            rect.size.h.round() as i32,
        )));
        tile.tiled_restore_pos = Some(pos);
        true
    }

    /// Puts back the geometry a displacement overrode, and whether the compositor is the one that
    /// maximized this window — both from a session record, on the map.
    ///
    /// The rect arrives output-local and in the window's frame, which is how the store keeps every
    /// rect; the tile holds it work-area-relative and in its own, so that a strut appearing does
    /// not skew what "where it was" means.
    ///
    /// `positioned` is false when the display the rect is local to did not come back. The size is
    /// kept anyway — it is what the window wants, wherever it lands — and the position is taken
    /// from wherever placement has just put the window, so growing the area later gives the size
    /// back without teleporting the window to coordinates from another display.
    pub fn seed_displaced_geometry(
        &mut self,
        id: &W::Id,
        rect: Option<Rectangle<f64, Logical>>,
        positioned: bool,
        auto_maximized: bool,
    ) -> bool {
        let area = self.floating.working_area();
        let placed = self.floating.logical_pos_of(id);

        let Some(tile) = self.tiles_mut().find(|tile| tile.window().id() == id) else {
            return false;
        };

        tile.auto_maximized = auto_maximized;
        tile.displaced_rect = rect.map(|rect| {
            let loc = if positioned {
                rect.loc - area.loc - tile.window_offset()
            } else {
                placed.unwrap_or(area.loc) - area.loc
            };
            Rectangle::new(
                loc,
                Size::from((
                    tile.tile_width_for_window_width(rect.size.w),
                    tile.tile_height_for_window_height(rect.size.h),
                )),
            )
        });
        true
    }

    /// What the session store remembers about `id`: how it is sized, the rect it would take if
    /// floating — mutter's `saved_rect` (`meta-wayland-xdg-session-state.c:32-57`) — and, when the
    /// compositor owns its geometry, the rect it actually occupies.
    ///
    /// Both rects are **output-local**, which is also the frame the store keeps them in. `None`
    /// for the floating rect means the window has never floated (it opened straight into maximize,
    /// say) and there is no remembered geometry to restore — how it is sized is still worth having.
    pub fn session_snapshot(&self, id: &W::Id) -> Option<TileSessionState> {
        let tile = self.tiles().find(|tile| tile.window().id() == id)?;

        // Where it actually sits, as the last resort — but only the floating layer has a position
        // worth saving; a scrolling-layer tile's is a column offset. Model values throughout,
        // never render positions: a window closed mid-animation must be remembered where the
        // layout has it, not where the animation had reached.
        let live = self
            .floating
            .tiles_with_offsets()
            .find(|(tile, _)| tile.window().id() == id)
            .map(|(tile, offset)| (offset, tile.window_size()));

        // `tiled_restore_*` first: that is the rect an un-maximize or an un-tile returns the
        // window to, and in GNOME mode — where the tile stays in the floating layer — it is the
        // one that holds the pre-maximize geometry. `floating_*` is the same memory for scrolling
        // mode, where the tile moved layers instead. Between them they are mutter's `saved_rect`.
        let pos = tile
            .tiled_restore_pos
            .or(tile.floating_pos)
            .map(|pos| self.floating.scale_by_working_area(pos))
            .or(live.map(|(offset, _)| offset));
        let size = tile
            .tiled_restore_size
            .or(tile.floating_window_size)
            .map(Size::to_f64)
            .or(live.map(|(_, size)| size));

        let rect = |pos, size| Rectangle::new(pos + tile.window_offset(), size);

        // The inverse of `seed_displaced_geometry`: back into the window's frame, and back into
        // output-local coordinates.
        let area = self.floating.working_area();
        let displaced_rect = tile.displaced_rect.map(|displaced| {
            Rectangle::new(
                displaced.loc + area.loc + tile.window_offset(),
                Size::from((
                    tile.window_width_for_tile_width(displaced.size.w),
                    tile.window_height_for_tile_height(displaced.size.h),
                )),
            )
        });

        Some(TileSessionState {
            sizing_mode: tile.sizing_mode(),
            edge_tiled: tile.window().edge_tiled_side(),
            floating_rect: pos.zip(size).map(|(pos, size)| rect(pos, size)),
            live_rect: live.map(|(offset, size)| rect(offset, size)),
            displaced_rect,
            auto_maximized: tile.auto_maximized,
        })
    }

    pub fn layout_config(&self) -> Option<&synoik_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub fn scrolling(&self) -> &ScrollingSpace<W> {
        &self.scrolling
    }

    #[cfg(test)]
    pub fn floating(&self) -> &FloatingSpace<W> {
        &self.floating
    }

    #[cfg(test)]
    pub fn verify_invariants(&self, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

        let scale = self.scale.fractional_scale();
        assert!(scale > 0.);
        assert!(scale.is_finite());

        let options = Options::clone(&self.base_options)
            .with_merged_layout(self.layout_config.as_ref())
            .adjusted_for_scale(scale);
        assert_eq!(
            &*self.options, &options,
            "options must be base options adjusted for scale"
        );

        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);

        assert_eq!(self.background_buffer.size(), self.view_size);
        assert_eq!(
            self.background_buffer.color().components(),
            options.layout.background_color.to_array_unpremul(),
        );

        assert_eq!(self.view_size, self.scrolling.view_size());
        assert_eq!(self.working_area, self.scrolling.parent_area());
        assert_eq!(&self.clock, self.scrolling.clock());
        assert!(Rc::ptr_eq(&self.options, self.scrolling.options()));
        self.scrolling.verify_invariants();

        assert_eq!(self.view_size, self.floating.view_size());
        assert_eq!(self.working_area, self.floating.working_area());
        assert_eq!(&self.clock, self.floating.clock());
        assert!(Rc::ptr_eq(&self.options, self.floating.options()));
        self.floating.verify_invariants();

        if self.floating.is_empty() {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        } else if self.scrolling.is_empty() {
            assert!(
                self.floating_is_active.get(),
                "when scrolling is empty but floating isn't, floating should be active"
            );
        }

        for (tile, tile_pos, visible) in self.tiles_with_render_positions() {
            if Some(tile.window().id()) != move_win_id {
                assert_eq!(tile.interactive_move_offset, Point::from((0., 0.)));
            }

            let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

            // Tile positions must be rounded to physical pixels.
            assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

            if let Some(alpha) = &tile.alpha_animation {
                let anim = &alpha.anim;
                if visible {
                    assert_eq!(anim.to(), 1., "visible tiles can animate alpha only to 1");
                }

                assert!(
                    !alpha.hold_after_done,
                    "tiles in the layout cannot have held alpha animation"
                );
            }
        }
    }
}

pub(super) fn compute_working_area(output: &Output, options: &Options) -> Rectangle<f64, Logical> {
    let mut area = layer_map_for_output(output).non_exclusive_zone().to_f64();

    // In GNOME (floating) mode the top panel reserves a strut at the top of the
    // output, exactly like gnome-shell's `set_builtin_struts`. Applying it here,
    // at the single working-area chokepoint, means maximize, edge-tiling,
    // floating placement and the overview picker slots all inset uniformly.
    if options.layout.windowing_mode == WindowingMode::Floating {
        let inset = crate::ui::panel::panel_height().min(area.size.h);
        area.loc.y += inset;
        area.size.h -= inset;
    }

    area
}

/// How much further the active workspace's shadow is thrown than its neighbours'.
const ACTIVE_SHADOW_REACH: f64 = 2.1;
/// …and how much denser. Clamped at fully opaque, which it does not reach at the default
/// `0x50`.
const ACTIVE_SHADOW_ALPHA: f32 = 2.;

/// [`compute_workspace_shadow_config`] thrown further and darker — the **active** workspace's
/// shadow in the thumbnail strip.
///
/// **Divergence (approved 2026-07-29; the accent dropped 2026-08-05).** gnome-shell marks the
/// active workspace with a border ring on the thumbnail (`.workspace-thumbnail-indicator`);
/// Gustavo asked for a shadow treatment instead.
///
/// It was the *accent* colour until now, and that was the wrong instinct — mine as much as
/// anyone's. A drop shadow says "this is above the surface" because it is darker than what it
/// falls on. Recolouring it to the accent keeps the shape and throws away the reason: with a
/// light accent (`yellow`, say) the halo comes out *brighter* than the backdrop, so the one
/// thumbnail that is supposed to be raised is the only one not casting a shadow. It read as
/// pushed back, which is exactly backwards, and no tint fraction fixes it — the top edge shows
/// only the softness bleed, where there is no shadow shape for a colour to live in, so any
/// accent strong enough to be recognisable turns that edge into an outline. Four intermediate
/// treatments were rendered and compared before settling here.
///
/// So the cue is depth, done with the only thing that expresses depth: the *same* shadow as
/// every other thumbnail, thrown [`ACTIVE_SHADOW_REACH`] further and
/// [`ACTIVE_SHADOW_ALPHA`] denser. Nothing about it depends on the accent, which means nothing
/// about it depends on the user's colour taste either.
pub(super) fn active_workspace_shadow_config(
    config: synoik_config::WorkspaceShadow,
    view_size: Size<f64, Logical>,
) -> synoik_config::Shadow {
    let mut config = compute_workspace_shadow_config(config, view_size);
    config.softness *= ACTIVE_SHADOW_REACH;
    config.spread *= ACTIVE_SHADOW_REACH;
    // Colors are stored unpremultiplied, so this is the shadow's own opacity.
    config.color.a = (config.color.a * ACTIVE_SHADOW_ALPHA).min(1.);
    config
}

pub(super) fn compute_workspace_shadow_config(
    config: synoik_config::WorkspaceShadow,
    view_size: Size<f64, Logical>,
) -> synoik_config::Shadow {
    // Gaps between workspaces are a multiple of the view height, so shadow settings should also be
    // normalized to the view height to prevent them from overlapping on lower resolutions.
    let norm = view_size.h / 1080.;

    let mut config = synoik_config::Shadow::from(config);
    config.softness *= norm;
    config.spread *= norm;
    config.offset.x.0 *= norm;
    config.offset.y.0 *= norm;

    config
}
