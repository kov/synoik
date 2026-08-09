// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::RefCell;
use std::cmp::{max, min};
use std::collections::HashMap;
use std::iter::zip;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::input::TouchSlot;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer, Texture};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::input::pointer::CursorIcon;
use smithay::output::{Output, WeakOutput};
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use synoik_config::{Action, Config};
use synoik_ipc::SizeChange;

use crate::animation::{Animation, Clock};
use crate::layout::expose;
use crate::layout::floating::DIRECTIONAL_MOVE_PX;
use crate::render_helpers::captured_texture::CapturedTextureRenderElement;
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::RenderTarget;
use crate::synoik_render_elements;
use crate::ui::widget::{self, style, Align, Painter, Rgba, ShapedText, TextShaper, TextStyle};
use crate::ui::window_preview::{CLOSE_BG, CLOSE_BG_HOVER};
use crate::utils::to_physical_precise_round;

/// Per-element cache of a neutral CPU buffer uploaded once to a `VkTexture` (see
/// [`upload_cached`]).
type VkCache = RefCell<Option<TextureBuffer<VkTexture>>>;

const SELECTION_BORDER: i32 = 2;

/// The stage's default font, GNOME points — the reference for the panel's `em` margin.
const BASE_FONT_PT: f64 = 11.;
/// Panel corner radius, logical px — GNOME `.screenshot-ui-panel` `$modal_radius * 2`
/// (`_screenshot.scss:9`). The whole panel is one rounded `%osd_panel` card.
const PANEL_RADIUS: f64 = 32.;

/// `.screenshot-ui-panel { margin-bottom: 4em }` (`_screenshot.scss:13`), logical px. `em` is the
/// element's own font size, which the panel inherits from the stage — so this is a font-relative
/// gap, not a fixed one, and it must not be hardcoded in px.
fn panel_margin_bottom() -> f64 {
    4. * crate::ui::pt_to_px(BASE_FONT_PT)
}

/// `%osd_panel`'s 1px inset border — GNOME's `$osd_outer_borders_color` = white@2%
/// (`_colors.scss:44`). The panel reads against the screenshot via its drop shadow, not a heavy
/// border. Its *fill* is [`style::OSD_BG`], which is also what makes the flat type buttons work:
/// a `%osd_button_flat` base is the container's own colour, not a hole in it.
const PANEL_BORDER_COLOR: [f32; 4] = [1., 1., 1., 0.02];

/// `.screenshot-ui-capture-button { border: 4px $osd_fg_color; padding: $base_margin }`
/// (`_screenshot.scss:44-45`) — a real ring with a transparent gap inside it, then the
/// `$large_icon_size` inner circle.
const CAPTURE_BORDER: f64 = 4.;
const CAPTURE_GAP: f64 = 4.;
const CAPTURE_INNER: f64 = 32.;

/// `.screenshot-ui-capture-button-circle` (`_screenshot.scss:48-64`): `$osd_fg_color` (white),
/// `darken(…, 20%)` on hover/focus, `darken(…, 50%)` while pressed.
const CAPTURE_CIRCLE: [f32; 4] = [1., 1., 1., 1.];
const CAPTURE_CIRCLE_HOVER: [f32; 4] = [0.8, 0.8, 0.8, 1.];
const CAPTURE_CIRCLE_ACTIVE: [f32; 4] = [0.5, 0.5, 0.5, 1.];

/// The same circle under `&:cast` (`_screenshot.scss:66-79`): `$screenshot_ui_button_red` is
/// `$red_4` (`#c01c28`), `lighten(…, 5%)` on hover and `darken(…, 7%)` while pressed. Lightness
/// steps are in HSL, which is why these are not a flat scale of the base.
const CAST_CIRCLE: [f32; 4] = [0.753, 0.110, 0.157, 1.];
const CAST_CIRCLE_HOVER: [f32; 4] = [0.840, 0.123, 0.175, 1.];
const CAST_CIRCLE_ACTIVE: [f32; 4] = [0.631, 0.092, 0.131, 1.];

/// `.screenshot-ui-close-button` extends `.window-close` but overrides its padding to
/// `$base_padding` (`_screenshot.scss:19`), so the box grows to fit the `$medium_icon_size` glyph
/// plus 6px a side rather than staying at `.window-close`'s 32px.
const CLOSE_SIZE: f64 = CLOSE_ICON_PX + 6. * 2.;
const CLOSE_ICON_PX: f64 = 24.;
/// `.left`/`.right { margin-*: $base_margin * 3 }` and `margin-top: $base_margin * 3`
/// (`_screenshot.scss:20-23`), logical px.
const CLOSE_MARGIN: f64 = 12.;

/// Drop shadow — GNOME `.screenshot-ui-panel` `box-shadow: 0 2px 4px 0 $shadow_color`
/// (`_screenshot.scss:21`); `$shadow_color` (dark) is `rgba(0,0,0,0.2)`. Logical px.
const SHADOW_COLOR: [f32; 4] = [0., 0., 0., 0.2];
const SHADOW_BLUR: f64 = 4.;
const SHADOW_OFFSET_Y: f64 = 2.;

// Ideally the screenshot UI should support cross-output selections. However, that poses some
// technical challenges when the outputs have different scales and such. So, this implementation
// allows only single-output selections for now.
//
// As a consequence of this, selection coordinates are in output-local coordinate space.
#[allow(clippy::large_enum_variant)]
pub enum ScreenshotUi {
    /// Closed, but still holding what the next open should come back to.
    ///
    /// gnome-shell's `ScreenshotUI` is a singleton built at startup that merely hides on close
    /// (`screenshot.js:1727`), so its controls keep their state for the life of the session — the
    /// capture type and the Show Pointer toggle are never touched by `_finishClosing`, and
    /// `AreaSelector.reset()` (`screenshot.js:304`) preserves the rectangle unless it has fallen
    /// out of bounds. This variant is that singleton's surviving fields. Nothing here reaches
    /// disk: GNOME has no GSettings key for any of it either, so a restart starts over.
    Closed {
        last_selection: Option<(WeakOutput, Rectangle<i32, Physical>)>,
        /// Remembered across opens, like GNOME's toggle button. Not remembered across restarts.
        show_pointer: bool,
        /// Remembered across opens. Falls back to `Selection` at open time if `Window` is picked
        /// but there is nothing to pick, mirroring `screenshot.js:1663-1664`.
        capture_type: CaptureType,
        clock: Clock,
        config: Rc<RefCell<Config>>,
    },
    Open {
        selection: (Output, Point<i32, Physical>, Point<i32, Physical>),
        output_data: HashMap<Output, OutputData>,
        button: Button,
        show_pointer: bool,
        /// Seconds to wait before the capture fires, cycling through [`DELAYS`]. Our divergence:
        /// GNOME's shell UI has no delay, only gnome-screenshot did.
        delay: u8,
        /// Deliberately *not* remembered across opens: GNOME resets to Shot on every close
        /// (`_shotButton.checked = true`, `screenshot.js:1739`), so the picker never comes back
        /// armed to record.
        mode: CaptureMode,
        capture_type: CaptureType,
        /// The window the selector has picked, as `(output, window id)`.
        ///
        /// Held by id rather than by index so it survives anything that reorders the list, and
        /// global rather than per-output because GNOME lets exactly one window be checked across
        /// every monitor (`screenshot.js:1643-1658` unchecks the others).
        selected_window: Option<(Output, u64)>,
        /// The window under the pointer, for the hover border.
        hovered_window: Option<(Output, u64)>,
        /// What the pointer should look like where it currently is. See [`Self::cursor_icon`].
        cursor: CursorIcon,
        /// The area rectangle put aside while another capture type borrows `selection`.
        ///
        /// Screen mode widens `selection` to the whole output so the capture path needs no special
        /// case, which would otherwise *destroy* the rectangle the user dragged. In GNOME nothing
        /// can: `_areaSelector` keeps its own geometry and Screen mode draws a different widget
        /// entirely (`_screenSelectors`), so a trip through Screen and back leaves the area
        /// exactly where it was.
        saved_area: Option<(Point<i32, Physical>, Point<i32, Physical>)>,
        /// The pending or showing tooltip, if the pointer has settled on a control.
        tooltip: Option<TooltipState>,
        /// The control under the pointer, on the **selection output's** panel.
        ///
        /// Motion only ever reaches us in the selection output's coordinate space (every call site
        /// in `input/mod.rs` computes it from `selection_output()`), so that is the one panel
        /// whose hover we can honestly track. Clicking a control on another output's panel
        /// still works — it just does not light up first.
        hover: Option<Control>,
        open_anim: Animation,
        clock: Clock,
        config: Rc<RefCell<Config>>,
        path: Option<String>,
    },
}

/// GNOME's tooltip appears 300ms after the pointer settles, then fades in over 150ms
/// (`Tooltip.open`, `js/ui/screenshot.js:95-119`). The delay is what keeps a pointer crossing the
/// panel from strobing seven tips on its way past.
/// The delays the button cycles through, in seconds. `0` is off. gnome-screenshot's own delay
/// spinner was free-form; a three-stop cycle is the most a single round button can carry, and
/// 3s/10s are the two stops its UI defaulted to.
const DELAYS: [u8; 3] = [0, 3, 10];

const TOOLTIP_DELAY: Duration = Duration::from_millis(300);
const TOOLTIP_FADE_MS: u64 = 150;
/// `.screenshot-ui-tooltip { -y-offset: $base_margin * 6 }` (`_screenshot.scss:202`) — how far
/// above its control the tip sits.
const TOOLTIP_Y_OFFSET: f64 = 24.;

/// A tooltip waiting out its delay, or fading in.
pub struct TooltipState {
    control: Control,
    /// When the tip becomes due. Before this it is not drawn at all.
    due: Duration,
    /// The fade, started once `due` passes. `None` while still waiting.
    fade: Option<Animation>,
}

/// State for moving the selection (as opposed to just drawing).
/// `.screenshot-ui-area-selector-handle` (`_screenshot.scss:131-137`): a `$medium_icon_size`
/// white circle with `box-shadow: 0 1px 3px 2px $shadow_color`, centred on each corner of the
/// selection. Logical px.
const HANDLE_PX: f64 = 24.;
const HANDLE_SHADOW_BLUR: f64 = 3.;
const HANDLE_SHADOW_SPREAD: f64 = 2.;
const HANDLE_SHADOW_OFFSET_Y: f64 = 1.;

/// How far outside an edge still grabs it: `10 * scaleFactor` (`js/ui/screenshot.js:375-376`).
/// Logical px.
const EDGE_GRAB_PX: f64 = 10.;

/// Which side of the selection one axis of a grab moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Left, or top.
    Low,
    /// Right, or bottom.
    High,
}

/// What part of the selection a point is on.
///
/// GNOME's `_computeCursorType` (`js/ui/screenshot.js:354-398`) is one function answering two
/// questions at once — what the cursor should be, and what a press there would grab — so this is
/// one type feeding both. Keeping them together is the point: a cursor that promises a drag the
/// press does not start is the bug this replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AreaTarget {
    /// Past the selection and its grab bands. A press drags a new rectangle out.
    Outside,
    /// Inside. A press moves the whole rectangle.
    Move,
    /// On an edge or a corner: which sides the pointer will drive. Never both `None`.
    Resize { x: Option<Side>, y: Option<Side> },
}

impl AreaTarget {
    /// The cursor that advertises this grab.
    pub fn cursor(self) -> CursorIcon {
        use Side::{High, Low};
        match self {
            Self::Outside => CursorIcon::Crosshair,
            Self::Move => CursorIcon::Move,
            Self::Resize { x, y } => match (x, y) {
                (Some(Low), Some(Low)) => CursorIcon::NwResize,
                (Some(High), Some(Low)) => CursorIcon::NeResize,
                (Some(Low), Some(High)) => CursorIcon::SwResize,
                (Some(High), Some(High)) => CursorIcon::SeResize,
                (Some(Low), None) => CursorIcon::WResize,
                (Some(High), None) => CursorIcon::EResize,
                (None, Some(Low)) => CursorIcon::NResize,
                (None, Some(High)) => CursorIcon::SResize,
                // `Resize` is only ever built with at least one side.
                (None, None) => CursorIcon::Move,
            },
        }
    }
}

/// Which side the moving edge is now on, given the stationary one.
fn side_of(moving: i32, stationary: i32) -> Side {
    if moving > stationary {
        Side::High
    } else {
        Side::Low
    }
}

/// Which part of `rect` the output-local **physical** `point` is on. `scale` carries GNOME's
/// logical thresholds into the same space.
pub fn area_target(
    rect: Rectangle<i32, Physical>,
    point: Point<i32, Physical>,
    scale: f64,
) -> AreaTarget {
    use Side::{High, Low};

    let (left, top) = (rect.loc.x, rect.loc.y);
    let (right, bottom) = (left + rect.size.w - 1, top + rect.size.h - 1);
    let (x, y) = (point.x, point.y);

    // The corner handles win, and they are hit-tested as **circles** of their own radius
    // (`:359-373`) — the corners of their bounding boxes are not grabs.
    let limit = (HANDLE_PX / 2. * scale).powi(2);
    let on_handle = |cx: i32, cy: i32| {
        let (dx, dy) = (f64::from(cx - x), f64::from(cy - y));
        dx * dx + dy * dy <= limit
    };
    for (cx, cy, sx, sy) in [
        (left, top, Low, Low),
        (right, top, High, Low),
        (left, bottom, Low, High),
        (right, bottom, High, High),
    ] {
        if on_handle(cx, cy) {
            return AreaTarget::Resize {
                x: Some(sx),
                y: Some(sy),
            };
        }
    }

    // Then the edge bands. Note they sit **outside** the rect: `leftX - x >= 0` means the pointer
    // is at or past the left edge going out (`:378`). Inside is strictly between the two.
    let threshold = (EDGE_GRAB_PX * scale).round() as i32;
    // `None` = not in any band on this axis, which is what makes the whole point `Outside`.
    let band = |low: i32, high: i32, v: i32| -> Option<Option<Side>> {
        if (0..=threshold).contains(&(low - v)) {
            Some(Some(Low))
        } else if (0..=threshold).contains(&(v - high)) {
            Some(Some(High))
        } else if low - v < 0 && v - high < 0 {
            Some(None)
        } else {
            None
        }
    };

    match (band(left, right, x), band(top, bottom, y)) {
        (Some(None), Some(None)) => AreaTarget::Move,
        (Some(x), Some(y)) => AreaTarget::Resize { x, y },
        _ => AreaTarget::Outside,
    }
}

pub struct MoveState {
    // Cursor offset from the selection's top-left when starting the move.
    pointer_offset: Point<i32, Physical>,
    // If the move is initiated by a touch, this is the slot. If `None`, the move was initiated by
    // holding Space.
    touch_slot: Option<TouchSlot>,
}

/// What a press on the area selector took hold of, mirroring GNOME's `_dragCursor`
/// (`js/ui/screenshot.js:456-537`).
pub enum Grab {
    /// Dragging a new rectangle out: `selection.2` follows the pointer.
    New,
    /// Moving the whole rectangle, size intact.
    ///
    /// Reachable two extra ways that are ours, not GNOME's, and kept as capabilities rather than
    /// as "niri's way of moving": holding Space, or a second touch, upgrades a `New` drag into a
    /// move mid-gesture. GNOME's own route — pressing *inside* the selection — now exists too.
    Move(MoveState),
    /// Resizing. `selection.1` holds the stationary sides and `selection.2` the moving ones, so
    /// these say only *which* of them the pointer drives; `None` means that axis is pinned.
    ///
    /// Not stored as a cursor the way GNOME stores it: the cursor is derived from where the moving
    /// corner currently is (see [`ScreenshotUi::drag_cursor`]), which is the same answer its
    /// `_onMotion` reaches by rewriting `_dragCursor` on every flip (`:672-709`) — without the
    /// chance of the two disagreeing.
    Resize { x: Option<Side>, y: Option<Side> },
}

impl Grab {
    /// The cursor this grab shows while it is held.
    ///
    /// For a resize it is *derived* from where the moving corner now is relative to the stationary
    /// one, so dragging a handle past the opposite side re-labels it — GNOME reaches the same
    /// place by rewriting `_dragCursor` on every motion (`js/ui/screenshot.js:672-709`).
    fn cursor(&self, start: Point<i32, Physical>, last: Point<i32, Physical>) -> CursorIcon {
        match self {
            Self::New => CursorIcon::Crosshair,
            Self::Move(_) => CursorIcon::Move,
            Self::Resize { x, y } => AreaTarget::Resize {
                x: x.map(|_| side_of(last.x, start.x)),
                y: y.map(|_| side_of(last.y, start.y)),
            }
            .cursor(),
        }
    }

    fn move_state(&self) -> Option<&MoveState> {
        match self {
            Self::Move(state) => Some(state),
            _ => None,
        }
    }
}

pub enum Button {
    Up,
    Down {
        touch_slot: Option<TouchSlot>,
        /// The panel control the press landed on, if any. A press that lands on a control acts on
        /// *release over the same control*, the way a button does everywhere else — so this is
        /// what the press is armed on, not merely "was it the capture button".
        on_control: Option<Control>,
        last_pos: (Output, Point<i32, Physical>),
        grab: Grab,
    },
}

/// What a press wants from the compositor beyond a redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerDown {
    /// Redraw and nothing else.
    Redraw,
    /// Warp the pointer to this output-local **physical** point — the edge or corner the press
    /// grabbed.
    ///
    /// GNOME warps too (`warp_pointer`, `js/ui/screenshot.js:519`), and it is not cosmetic: an
    /// edge can be grabbed from up to 10px outside it, so without the warp every later delta
    /// would be measured from a pointer sitting beside the thing it is dragging.
    WarpTo(Point<i32, Physical>),
}

/// What a release did, for the caller to act on. Everything the panel can settle by itself (a mode
/// change, a toggle) it settles inside [`ScreenshotUi::pointer_up`] and reports as `Redraw`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerUp {
    /// The capture button fired.
    Capture,
    /// The close button was clicked — the caller must go through `Synoik::close_screenshot_ui`,
    /// which is also what answers a waiting D-Bus caller.
    Close,
    /// Something changed on screen and nothing else.
    Redraw,
}

pub struct OutputData {
    size: Size<i32, Physical>,
    scale: f64,
    transform: Transform,
    // Output, screencast, screen capture.
    screenshot: [OutputScreenshot; 3],
    buffers: [SolidColorBuffer; 8],
    locations: [Point<i32, Physical>; 8],
    /// The control panel, drawn straight into `VkTexture`s by the owned renderer and cached across
    /// frames. Built lazily on the first render (no renderer is available at open time); the hit
    /// test reads its layout, so no control is clickable until one frame has drawn.
    panel: RefCell<PanelCache>,
    /// This output's windows, frozen when the picker opened, and where each is drawn in the
    /// Window-mode selector. Parallel vectors, indexed together; empty on an output with no
    /// windows on its active workspace.
    windows: Vec<WindowShot>,
    slots: Vec<Rectangle<f64, Logical>>,
    /// The tooltip pill. One cache per output, re-baked when the text changes (the revision is
    /// the text's hash) — there is only ever one tip up.
    tooltip_cache: RefCell<widget::BakeCache>,
}

/// The caption metrics a content-sized panel must have before it can be laid out: each type
/// button's caption width and the shared line height, in logical px.
///
/// Split out of the bake because measuring means shaping and shaping means a renderer — which was
/// the one thing standing between the headless corpus and driving a real panel control. The corpus
/// supplies these directly through [`ScreenshotUi::lay_out_panels`]; everything downstream of the
/// layout is the production path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptionMetrics {
    pub label_w: [f64; 3],
    pub label_h: f64,
}

#[cfg(test)]
impl CaptionMetrics {
    /// Plausible metrics at scale 1, for tests that have no shaper.
    ///
    /// The three widths differ, which is what makes the homogeneous rule observable, and the
    /// **widest is Window** — the button most likely to be insensitive — so a layout that sized the
    /// row from only the sensitive captions fails rather than passing by luck.
    pub const TEST: Self = Self {
        label_w: [58., 42., 66.],
        label_h: 13.,
    };
}

/// Shot or cast: the two halves of the panel's segmented control (`screenshot.js:1360-1391`).
///
/// This is *how* the capture is taken, as against [`CaptureType`]'s *what* — the same area can be
/// photographed or recorded, which is why they are separate controls in GNOME and separate state
/// here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureMode {
    #[default]
    Shot,
    Cast,
}

impl CaptureMode {
    /// The segment index this mode occupies, and the reverse.
    pub fn index(self) -> usize {
        match self {
            Self::Shot => 0,
            Self::Cast => 1,
        }
    }

    fn from_index(i: usize) -> Self {
        if i == 0 {
            Self::Shot
        } else {
            Self::Cast
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Shot => "camera-photo-symbolic",
            Self::Cast => "camera-web-symbolic",
        }
    }
}

/// What an armed delayed capture will shoot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingTarget {
    /// An output-local **physical** crop; the whole output in Screen mode.
    Area(Rectangle<i32, Physical>),
    /// A window by id, re-found at fire time so it can have moved meanwhile.
    Window(u64),
}

/// The panel state the bake depends on. Anything else — the selection, the animation, the pointer
/// wandering over the screenshot — leaves the texture alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanelState {
    capture_type: CaptureType,
    /// Whether the Window button is sensitive — GNOME keeps it *visible* and greys it out when
    /// there is nothing to pick (`_syncWindowButtonSensitivity`, `js/ui/screenshot.js:1529-1536`).
    window_enabled: bool,
    show_pointer: bool,
    delay: u8,
    mode: CaptureMode,
    hover: Option<Control>,
    /// The control currently held down, for the `:active` fills.
    active: Option<Control>,
}

/// The baked panel, rebuilt on a [`PanelState`], scale or renderer-context change.
#[derive(Default)]
struct PanelCache {
    scale: f64,
    context: Option<ContextId<VkTexture>>,
    /// What [`texture`](Self::texture) was baked for; `None` before the first bake.
    state: Option<PanelState>,
    texture: Option<VkTexture>,
    /// Where every control sits inside `texture`, in panel-local logical px. Produced by the bake
    /// because it is content-sized (the captions need a shaper), and it is the *only* hit-test
    /// authority — a control cannot be drawn somewhere the pointer will not find it.
    layout: Option<PanelLayout>,
    /// The panel's drop shadow, baked into its own transparent texture and composited *behind* the
    /// panel.
    shadow: Option<VkTexture>,
    /// The close button, which is a sibling of the panel rather than a child of it (it straddles a
    /// panel corner), so it is its own texture placed by [`close_rect`].
    close: Option<VkTexture>,
    /// One selection corner handle. All four are identical, so this is baked once and drawn four
    /// times — it depends on the scale alone, never on [`PanelState`].
    handle: Option<VkTexture>,
}

impl PanelState {
    /// Whether `ty` can be picked right now. Only Window is ever refused, and only for want of a
    /// window to pick.
    fn enables(self, ty: CaptureType) -> bool {
        ty != CaptureType::Window || self.window_enabled
    }
}

/// A type button's glyph + caption colour. `%osd_button_flat:insensitive` halves the foreground
/// and leaves the fill alone (`_drawing.scss:296-300`, `$tc` = `$osd_fg_color`).
fn type_fg(enabled: bool) -> Rgba {
    if enabled {
        style::OSD_FG
    } else {
        [
            style::OSD_FG[0],
            style::OSD_FG[1],
            style::OSD_FG[2],
            style::OSD_FG[3] * 0.5,
        ]
    }
}

impl PanelCache {
    /// The panel's physical size, or `None` before it has been laid out.
    ///
    /// Derived from the **layout**, not from the texture: `generate_panel` sizes the texture as
    /// exactly `physical_size(scale, layout.size)`, so reading it back off the texture was a second
    /// source for a number the layout already holds — and one that does not exist until a renderer
    /// has baked. Going through the layout is what lets a panel be placed and hit-tested without
    /// one (see [`ScreenshotUi::lay_out_panels`]).
    fn size(&self) -> Option<Size<i32, Buffer>> {
        let logical = self.layout?.size;
        let size = widget::physical_size(self.scale, logical);
        Some(Size::from((size.w, size.h)))
    }
}

/// An output's screenshot captured through the owned Vulkan renderer up front, so a Vulkan session
/// never bakes (nor reads back) a GLES texture for the screenshot UI at all.
///
/// Captured once per render target — the frozen screen is drawn into screencasts and screen
/// captures too, and those differ by block-out rules. A missing `screen` drops the output; a
/// missing `pointer` only costs the cursor. Empty (via `Default`) on a GLES session.
#[derive(Default)]
pub struct ScreenshotNeutral {
    pub screen: Option<MemoryBuffer>,
    pub pointer: Option<(MemoryBuffer, Point<f64, Logical>)>,
}

/// One window, frozen at picker-open time, for the Window-mode selector.
///
/// GNOME's `UIWindowSelectorWindowContent` (`js/ui/screenshot.js:829-975`) holds the same three
/// things: the captured content, the frame rect it was captured at (`boundingBox`, which drives
/// both the layout and the aspect ratio the thumbnail is drawn at), and the identity to hand back.
pub struct WindowShot {
    /// The window id, so a selection survives the list being rebuilt.
    id: u64,
    /// The window's output-local logical rect when the picker opened — the layout's input, and
    /// the aspect the thumbnail keeps.
    rect: Rectangle<f64, Logical>,
    neutral: MemoryBuffer,
    neutral_vk: VkCache,
}

impl WindowShot {
    pub fn new(id: u64, rect: Rectangle<f64, Logical>, neutral: MemoryBuffer) -> Self {
        Self {
            id,
            rect,
            neutral,
            neutral_vk: RefCell::new(None),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// The thumbnail, scaled to fill `slot`.
    ///
    /// The capture is the window's *buffer*, which can be larger than the frame rect the slot was
    /// sized from (shadows, CSD margins). GNOME allocates the actor at `bufferRect` scaled by the
    /// slot/boundingBox ratio and lets it overhang (`vfunc_allocate`,
    /// `js/ui/screenshot.js:911-928`); the same ratio is what this applies.
    fn element(
        &self,
        renderer: &mut VulkanRenderer,
        slot: Rectangle<f64, Logical>,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let tb = upload_cached(renderer, &self.neutral, &self.neutral_vk)?;
        let natural = tb.logical_size();
        if natural.w <= 0. || natural.h <= 0. || self.rect.size.w <= 0. || self.rect.size.h <= 0. {
            return None;
        }

        // One uniform ratio, from the frame rect the slot was computed for.
        let ratio = f64::min(
            slot.size.w / self.rect.size.w,
            slot.size.h / self.rect.size.h,
        );
        let drawn = Size::from((natural.w * ratio, natural.h * ratio));
        // Centre the buffer on the slot: the frame rect sits inside the buffer, so anchoring at
        // the slot's corner would push the visible window off by the shadow margin.
        let loc = Point::from((
            slot.loc.x + (slot.size.w - drawn.w) / 2.,
            slot.loc.y + (slot.size.h - drawn.h) / 2.,
        ));

        let mut elem = TextureRenderElement::from_texture_buffer(
            tb,
            loc,
            alpha,
            None,
            None,
            Kind::Unspecified,
        );
        elem.set_size(drawn);
        Some(CapturedTextureRenderElement(elem))
    }
}

/// One output's frozen screen (and pointer + its logical location), as renderer-neutral CPU
/// buffers. Saving to disk crops these on the CPU rather than reading back from the GPU.
pub struct OutputScreenshot {
    screen: MemoryBuffer,
    pointer: Option<(MemoryBuffer, Point<f64, Logical>)>,
    /// `screen` / `pointer` uploaded once to `VkTexture`s, cached across frames.
    screen_vk: VkCache,
    pointer_vk: VkCache,
}

synoik_render_elements! {
    ScreenshotUiRenderElement => {
        Screenshot = CapturedTextureRenderElement,
        SolidColor = SolidColorRenderElement,
    }
}

impl Button {
    fn is_down(&self) -> bool {
        matches!(self, Self::Down { .. })
    }

    fn is_dragging_selection(&self) -> bool {
        matches!(
            self,
            Self::Down {
                on_control: None,
                ..
            }
        )
    }

    /// The control the press is armed on, if any.
    fn on_control(&self) -> Option<Control> {
        match self {
            Self::Up => None,
            Self::Down { on_control, .. } => *on_control,
        }
    }
}

impl ScreenshotUi {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self::Closed {
            last_selection: None,
            // GNOME's button is constructed unchecked (`screenshot.js:1417-1421`) and the cursor
            // actor starts hidden, so a first-ever picker shows no pointer.
            show_pointer: false,
            capture_type: CaptureType::default(),
            clock,
            config,
        }
    }

    pub fn open(
        &mut self,
        // Output, screencast, screen capture.
        screenshots: HashMap<Output, [OutputScreenshot; 3]>,
        mut window_shots: HashMap<Output, Vec<WindowShot>>,
        default_output: Output,
        focused_window: Option<u64>,
        path: Option<String>,
    ) -> bool {
        if screenshots.is_empty() {
            // Every output's capture failed (each warned individually). Say so, or the keybind
            // just silently does nothing.
            warn!("no output could be captured; not opening the screenshot UI");
            return false;
        }

        let Self::Closed {
            last_selection,
            show_pointer,
            capture_type: remembered_type,
            clock,
            config,
        } = self
        else {
            return false;
        };
        let show_pointer = *show_pointer;
        let remembered_type = *remembered_type;

        let last_selection = last_selection
            .take()
            .and_then(|(weak, sel)| weak.upgrade().map(|output| (output, sel)));
        let selection = match last_selection {
            Some(selection) if screenshots.contains_key(&selection.0) => selection,
            _ => {
                let output = default_output;
                let output_transform = output.current_transform();
                let output_mode = output.current_mode().unwrap();
                let size = output_transform.transform_size(output_mode.size);
                (
                    output,
                    Rectangle::new(
                        Point::from((size.w / 4, size.h / 4)),
                        Size::from((size.w / 2, size.h / 2)),
                    ),
                )
            }
        };

        let selection = (
            selection.0,
            selection.1.loc,
            selection.1.loc + selection.1.size - Size::from((1, 1)),
        );

        let output_data: HashMap<Output, OutputData> = screenshots
            .into_iter()
            .map(|(output, screenshot)| {
                let transform = output.current_transform();
                let output_mode = output.current_mode().unwrap();
                let size = transform.transform_size(output_mode.size);
                let scale = output.current_scale().fractional_scale();
                let buffers = [
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [1., 1., 1., 1.]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                    SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.5]),
                ];
                let locations = [Default::default(); 8];

                let windows = window_shots.remove(&output).unwrap_or_default();
                // The selector's slots come from the same layout strategy the overview picker
                // uses (`UIWindowSelectorLayout extends WorkspaceLayout`,
                // `js/ui/screenshot.js:764`), over the area GNOME leaves it — the full monitor
                // less a 100px margin, and 200px at the bottom on the monitor holding the panel
                // (`_screenshot.scss:142-151`).
                let area = window_selector_area(size.to_f64().to_logical(scale));
                let rects: Vec<_> = windows.iter().map(|w| w.rect).collect();
                let slots = expose::compute_slots(f64::from(size.h) / scale, area, &rects);

                let data = OutputData {
                    size,
                    scale,
                    transform,
                    screenshot,
                    buffers,
                    locations,
                    panel: RefCell::new(PanelCache::default()),
                    windows,
                    slots,
                    tooltip_cache: RefCell::new(widget::BakeCache::default()),
                };
                (output, data)
            })
            .collect();

        // GNOME checks the focused window up front and takes it out of toggle mode, so the
        // selector opens on something rather than on nothing (`screenshot.js:1088-1091`). Falling
        // back to the first window on the default output keeps that true when focus is elsewhere
        // (a layer surface, or nothing at all).
        let selected_window = focused_window
            .filter(|id| {
                output_data
                    .values()
                    .any(|d: &OutputData| d.windows.iter().any(|w| w.id == *id))
            })
            .and_then(|id| {
                output_data
                    .iter()
                    .find(|(_, d)| d.windows.iter().any(|w| w.id == id))
                    .map(|(output, _)| (output.clone(), id))
            })
            .or_else(|| {
                output_data
                    .iter()
                    .find(|(_, d)| !d.windows.is_empty())
                    .map(|(output, d)| (output.clone(), d.windows[0].id))
            });

        let open_anim = {
            let c = config.borrow();
            Animation::new(clock.clone(), 0., 1., 0., c.animations.screenshot_ui_open.0)
        };

        *self = Self::Open {
            selection,
            output_data,
            button: Button::Up,
            show_pointer,
            delay: 0,
            mode: CaptureMode::default(),
            capture_type: CaptureType::default(),
            selected_window,
            hovered_window: None,
            cursor: CursorIcon::Crosshair,
            saved_area: None,
            tooltip: None,
            hover: None,
            open_anim,
            clock: clock.clone(),
            config: config.clone(),
            path,
        };

        self.update_buffers();

        // Applied after the state exists so the restore goes through the same door a click does:
        // Screen has to widen the selection and put the area aside, and Window has to decline when
        // there is nothing to pick. `set_capture_type` is a no-op for `Selection`, which is the
        // common case, so the usual open still bakes once.
        self.set_capture_type(remembered_type);

        true
    }

    pub fn close(&mut self) -> bool {
        let Self::Open {
            selection,
            saved_area,
            show_pointer,
            capture_type,
            clock,
            config,
            ..
        } = self
        else {
            return false;
        };

        // Screen and Window mode borrow `selection`, so what the user dragged is in `saved_area`.
        // Remembering `selection` regardless would hand the next open a whole-output rectangle and
        // silently eat the area — in GNOME the area selector is a separate widget that Screen mode
        // never touches at all.
        let area = saved_area.unwrap_or((selection.1, selection.2));
        let last_selection = Some((
            selection.0.downgrade(),
            rect_from_corner_points(area.0, area.1),
        ));

        *self = Self::Closed {
            last_selection,
            show_pointer: *show_pointer,
            capture_type: *capture_type,
            clock: clock.clone(),
            config: config.clone(),
        };

        true
    }

    pub fn toggle_pointer(&mut self) {
        if let Self::Open { show_pointer, .. } = self {
            *show_pointer = !*show_pointer;
        }
    }

    /// Step to the next delay in [`DELAYS`], wrapping back to off.
    fn cycle_delay(&mut self) {
        if let Self::Open { delay, .. } = self {
            let next = DELAYS.iter().position(|d| d == delay).map_or(0, |i| i + 1);
            *delay = DELAYS[next % DELAYS.len()];
        }
    }

    /// How long the capture should wait before it fires, or `None` for right now.
    pub fn delay(&self) -> Option<Duration> {
        match self {
            Self::Open { delay: 0, .. } | Self::Closed { .. } => None,
            Self::Open { delay, .. } => Some(Duration::from_secs(u64::from(*delay))),
        }
    }

    /// Switch what the capture button will act on.
    ///
    /// `Screen` takes the whole output as the selection, so the shade collapses and the ordinary
    /// capture path needs no special case — the rect it crops is simply the full frame.
    pub fn set_capture_type(&mut self, ty: CaptureType) {
        let window_enabled = self.window_enabled();
        let Self::Open {
            capture_type,
            selection,
            output_data,
            saved_area,
            ..
        } = self
        else {
            return;
        };

        if *capture_type == ty {
            return;
        }
        // An insensitive button must not switch modes even if a click somehow reaches it.
        if ty == CaptureType::Window && !window_enabled {
            return;
        }

        // Leaving Selection puts the area aside; returning brings it back.
        if *capture_type == CaptureType::Selection {
            *saved_area = Some((selection.1, selection.2));
        } else if ty == CaptureType::Selection {
            if let Some((a, b)) = saved_area.take() {
                selection.1 = a;
                selection.2 = b;
            }
        }
        *capture_type = ty;

        if ty == CaptureType::Screen {
            let size = output_data[&selection.0].size;
            selection.1 = Point::from((0, 0));
            selection.2 = Point::from((size.w - 1, size.h - 1));
        }

        self.update_buffers();
    }

    /// The tooltip currently *drawn*, or `None` while one is still waiting out its delay.
    ///
    /// Test-only, and deliberately reports the drawn state rather than the pending one: a tip
    /// scheduled but not yet due is invisible, and a test that could not tell those apart would
    /// pass on the bug this exists to catch.
    #[cfg(test)]
    pub fn tooltip_text(&self) -> Option<&'static str> {
        let Self::Open { tooltip, .. } = self else {
            return None;
        };
        let state = tooltip.as_ref()?;
        state.fade.as_ref()?;
        state.control.tooltip()
    }

    /// Lay every open output's panel out from supplied caption metrics, with no renderer.
    ///
    /// The panel is content-sized, so its `PanelLayout` normally falls out of the bake — and the
    /// bake needs a `TextShaper`, hence a Vulkan device. That is the only reason driving a panel
    /// control ever required one: `control_at` reads the cached layout, and without it every
    /// button is unreachable. This installs the layout and nothing else, so the hit test, the
    /// activation and every state transition downstream stay the production path.
    ///
    /// Nothing is drawn: with no texture, `panel_element` yields nothing and the panel simply does
    /// not appear. A later render with a real renderer re-bakes over this.
    ///
    /// For the headless corpus (`src/tests/gnome.rs`), which has no device.
    #[cfg(test)]
    pub fn lay_out_panels(&mut self, metrics: CaptionMetrics) {
        let Self::Open { output_data, .. } = self else {
            return;
        };
        for data in output_data.values() {
            let mut cache = data.panel.borrow_mut();
            cache.scale = data.scale;
            cache.layout = Some(PanelLayout::new(metrics));
        }
    }

    /// Whether any output has a window to pick — what makes the Window button sensitive.
    ///
    /// GNOME asks the same question of every selector at once (`_syncWindowButtonSensitivity`,
    /// `js/ui/screenshot.js:1529-1536`), so a second monitor's windows keep the button live.
    pub fn any_windows(&self) -> bool {
        match self {
            Self::Open { output_data, .. } => output_data.values().any(|d| !d.windows.is_empty()),
            Self::Closed { .. } => false,
        }
    }

    /// Whether Window mode can be entered — the one authority, read by the bake, the hit test's
    /// hover filter and [`Self::set_capture_type`].
    ///
    /// Cast mode refuses it outright: recording a single window is not something GNOME's recorder
    /// does either (`_startScreencast` returns early on `_windowButton.checked`,
    /// `js/ui/screenshot.js:2013`, and `_onCastButtonToggled` greys the button rather than leaving
    /// a mode that silently does nothing).
    pub fn window_enabled(&self) -> bool {
        self.mode() == CaptureMode::Shot && self.any_windows()
    }

    /// What the pointer should look like where it currently is.
    ///
    /// Tracked rather than asked per-call because it falls out of the same hit test as the hover,
    /// and it shares that hit test's one honest limitation: motion only ever reaches us in the
    /// selection output's coordinate space, so a second monitor's panel does not steer it.
    pub fn cursor_icon(&self) -> CursorIcon {
        match self {
            Self::Open { cursor, .. } => *cursor,
            Self::Closed { .. } => CursorIcon::Default,
        }
    }

    pub fn mode(&self) -> CaptureMode {
        match self {
            Self::Open { mode, .. } => *mode,
            Self::Closed { .. } => CaptureMode::default(),
        }
    }

    /// Switch between photographing and recording.
    ///
    /// Cast mode drops the frozen screen (the recording is of the *live* one, and a still of the
    /// moment the picker opened would be a lie about what is being recorded) and takes Window mode
    /// with it, falling back to Selection if that is where we were — GNOME does both in
    /// `_onCastButtonToggled` (`js/ui/screenshot.js:1880-1906`).
    pub fn set_mode(&mut self, new: CaptureMode) {
        if self.mode() == new {
            return;
        }
        if let Self::Open { mode, .. } = self {
            *mode = new;
        }
        if new == CaptureMode::Cast && self.capture_type() == CaptureType::Window {
            self.set_capture_type(CaptureType::Selection);
        }
        self.update_buffers();
    }

    /// The window the selector has picked, as `(output, id)`.
    pub fn selected_window(&self) -> Option<(&Output, u64)> {
        match self {
            Self::Open {
                selected_window, ..
            } => selected_window.as_ref().map(|(o, id)| (o, *id)),
            Self::Closed { .. } => None,
        }
    }

    /// What a delayed capture should shoot when its timer runs out, as `(output, target)`.
    ///
    /// Resolved *while the picker is still up*, because arming dismisses it: nothing about the
    /// selection survives the close. Window mode hands over an id rather than a rect — the window
    /// is free to move during the delay, and shooting where it *was* is not what was asked for.
    pub fn pending_target(&self) -> Option<(Output, PendingTarget)> {
        let Self::Open {
            selection,
            output_data,
            capture_type,
            ..
        } = self
        else {
            return None;
        };

        if *capture_type == CaptureType::Window {
            let (output, id) = self.selected_window()?;
            return Some((output.clone(), PendingTarget::Window(id)));
        }

        // Screen mode has already widened the selection to the whole output, so both modes are the
        // same crop here.
        let (output, a, b) = selection;
        output_data.get(output)?;
        Some((
            output.clone(),
            PendingTarget::Area(rect_from_corner_points(*a, *b)),
        ))
    }

    /// Whether the capture should include the pointer.
    ///
    /// Answers while closed too, with what the next open will come back to.
    pub fn show_pointer(&self) -> bool {
        match self {
            Self::Open { show_pointer, .. } | Self::Closed { show_pointer, .. } => *show_pointer,
        }
    }

    /// What the capture button acts on — likewise remembered while closed.
    pub fn capture_type(&self) -> CaptureType {
        match self {
            Self::Open { capture_type, .. } | Self::Closed { capture_type, .. } => *capture_type,
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, ScreenshotUi::Open { .. })
    }

    pub fn set_space_down(&mut self, down: bool) {
        if let Self::Open {
            selection,
            button: Button::Down { grab, last_pos, .. },
            ..
        } = self
        {
            if down {
                // Only a new-rectangle drag is upgraded. A resize already has hold of a side, and
                // turning that into a move mid-gesture would abandon it silently.
                if matches!(grab, Grab::New) {
                    *grab = Grab::Move(MoveState {
                        pointer_offset: last_pos.1 - selection.1,
                        touch_slot: None,
                    });
                }
            } else if matches!(
                grab,
                Grab::Move(MoveState {
                    touch_slot: None,
                    ..
                })
            ) {
                // Only give it back if the move was Space's doing.
                *grab = Grab::New;
            }
        }
    }

    pub fn move_left(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            // Screen mode's selection *is* the output; nudging or resizing it would silently
            // make the panel lie about what the capture button will take.
            capture_type: CaptureType::Selection,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, min(a.x, b.x));
        a.x -= delta;
        b.x -= delta;

        self.update_buffers();
    }

    pub fn move_right(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            // Screen mode's selection *is* the output; nudging or resizing it would silently
            // make the panel lie about what the capture button will take.
            capture_type: CaptureType::Selection,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, data.size.w - max(a.x, b.x) - 1);
        a.x += delta;
        b.x += delta;

        self.update_buffers();
    }

    pub fn move_up(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            // Screen mode's selection *is* the output; nudging or resizing it would silently
            // make the panel lie about what the capture button will take.
            capture_type: CaptureType::Selection,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, min(a.y, b.y));
        a.y -= delta;
        b.y -= delta;

        self.update_buffers();
    }

    pub fn move_down(&mut self) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            // Screen mode's selection *is* the output; nudging or resizing it would silently
            // make the panel lie about what the capture button will take.
            capture_type: CaptureType::Selection,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let delta: i32 = to_physical_precise_round(data.scale, DIRECTIONAL_MOVE_PX);
        let delta = min(delta, data.size.h - max(a.y, b.y) - 1);
        a.y += delta;
        b.y += delta;

        self.update_buffers();
    }

    /// Moves the screenshot selection to a different output.
    ///
    /// This preserves the relative position while keeping logical size. It is (intentionally) very
    /// similar to how floating windows move across monitors, but with one difference: floating
    /// windows can go partially outside the view, while the screenshot selection cannot. So, we
    /// clamp it to new output bounds, trying to preserve the size if possible.
    pub fn move_to_output(&mut self, new_output: Output) {
        let Self::Open {
            selection,
            output_data,
            capture_type,
            ..
        } = self
        else {
            return;
        };

        let (current_output, current_a, current_b) = selection;

        if current_output == &new_output {
            return;
        }

        let Some(target_data) = output_data.get(&new_output) else {
            return;
        };

        // Screen mode selects the whole output, so moving means taking all of the new one — not
        // carrying a proportional rect across.
        if *capture_type == CaptureType::Screen {
            let size = target_data.size;
            *selection = (
                new_output,
                Point::from((0, 0)),
                Point::from((size.w - 1, size.h - 1)),
            );
            self.update_buffers();
            return;
        }

        let current_data = &output_data[current_output];

        let current_rect: Rectangle<_, Physical> = Rectangle::new(
            Point::from((current_a.x.min(current_b.x), current_a.y.min(current_b.y))),
            Size::from((
                (current_a.x.max(current_b.x) - current_a.x.min(current_b.x) + 1),
                (current_a.y.max(current_b.y) - current_a.y.min(current_b.y) + 1),
            )),
        );
        let current_rect = current_rect.to_f64();

        let rel_x = current_rect.loc.x / current_data.size.w as f64;
        let rel_y = current_rect.loc.y / current_data.size.h as f64;

        let factor = target_data.scale / current_data.scale;
        let mut new_width = (current_rect.size.w * factor).round() as i32;
        let mut new_height = (current_rect.size.h * factor).round() as i32;

        new_width = new_width.clamp(1, target_data.size.w);
        new_height = new_height.clamp(1, target_data.size.h);

        let new_x = (rel_x * target_data.size.w as f64).round() as i32;
        let new_y = (rel_y * target_data.size.h as f64).round() as i32;

        let max_x = target_data.size.w - new_width;
        let max_y = target_data.size.h - new_height;
        let new_x = new_x.clamp(0, max_x);
        let new_y = new_y.clamp(0, max_y);

        let new_rect = Rectangle::new(
            Point::from((new_x, new_y)),
            Size::from((new_width, new_height)),
        );

        *selection = (
            new_output,
            new_rect.loc,
            new_rect.loc + new_rect.size - Size::from((1, 1)),
        );

        self.update_buffers();
    }

    pub fn set_width(&mut self, change: SizeChange) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            // Screen mode's selection *is* the output; nudging or resizing it would silently
            // make the panel lie about what the capture button will take.
            capture_type: CaptureType::Selection,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let available_size = f64::from(data.size.w);
        let current_size = max(a.x, b.x) + 1 - min(a.x, b.x);

        let new_size = match change {
            SizeChange::SetFixed(fixed) => to_physical_precise_round(data.scale, fixed),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
            SizeChange::AdjustFixed(delta) => {
                let delta = to_physical_precise_round(data.scale, delta);
                current_size.saturating_add(delta)
            }
            SizeChange::AdjustProportion(delta) => {
                let current_prop = f64::from(current_size) / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
        };
        let new_size = new_size.clamp(1, data.size.w - min(a.x, b.x)) - 1;
        a.x = min(a.x, b.x);
        b.x = a.x + new_size;

        self.update_buffers();
    }

    pub fn set_height(&mut self, change: SizeChange) {
        let Self::Open {
            selection: (output, a, b),
            output_data,
            // Screen mode's selection *is* the output; nudging or resizing it would silently
            // make the panel lie about what the capture button will take.
            capture_type: CaptureType::Selection,
            ..
        } = self
        else {
            return;
        };

        let data = &output_data[output];

        let available_size = f64::from(data.size.h);
        let current_size = max(a.y, b.y) + 1 - min(a.y, b.y);

        let new_size = match change {
            SizeChange::SetFixed(fixed) => to_physical_precise_round(data.scale, fixed),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
            SizeChange::AdjustFixed(delta) => {
                let delta = to_physical_precise_round(data.scale, delta);
                current_size.saturating_add(delta)
            }
            SizeChange::AdjustProportion(delta) => {
                let current_prop = f64::from(current_size) / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., 1.);
                (available_size * prop).round() as i32
            }
        };
        let new_size = new_size.clamp(1, data.size.h - min(a.y, b.y)) - 1;
        a.y = min(a.y, b.y);
        b.y = a.y + new_size;

        self.update_buffers();
    }

    pub fn advance_animations(&mut self) {
        let Self::Open { tooltip, clock, .. } = self else {
            return;
        };

        let Some(state) = tooltip else {
            return;
        };
        if state.fade.is_none() && clock.now_unadjusted() >= state.due {
            state.fade = Some(Animation::ease(
                clock.clone(),
                0.,
                1.,
                0.,
                TOOLTIP_FADE_MS,
                crate::animation::Curve::EaseOutQuad,
            ));
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        let Self::Open {
            open_anim, tooltip, ..
        } = self
        else {
            return false;
        };

        // A tooltip still counting down its delay has no animation yet, but the loop must keep
        // ticking or it never becomes due — a tip that only appears when something else happens
        // to redraw is worse than none.
        let tooltip_pending = tooltip
            .as_ref()
            .is_some_and(|t| t.fade.as_ref().is_none_or(|a| !a.is_done()));

        !open_anim.is_done() || tooltip_pending
    }

    fn update_buffers(&mut self) {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            panic!("screenshot UI must be open to update buffers");
        };

        let (selection_output, a, b) = selection;
        let mut rect = rect_from_corner_points(*a, *b);

        for (output, data) in output_data {
            let buffers = &mut data.buffers;
            let locations = &mut data.locations;
            let size = data.size;
            let scale = data.scale;

            if output == selection_output {
                // Check if the selection is still valid. If not, reset it back to default.
                if !Rectangle::from_size(size).contains_rect(rect) {
                    rect = Rectangle::new(
                        Point::from((size.w / 4, size.h / 4)),
                        Size::from((size.w / 2, size.h / 2)),
                    );
                    *a = rect.loc;
                    *b = rect.loc + rect.size - Size::from((1, 1));
                }

                let border = to_physical_precise_round(scale, SELECTION_BORDER);

                let resize = move |buffer: &mut SolidColorBuffer, w: i32, h: i32| {
                    let size = Size::<_, Physical>::from((w, h));
                    buffer.resize(size.to_f64().to_logical(scale));
                };

                resize(&mut buffers[0], rect.size.w + border * 2, border);
                resize(&mut buffers[1], rect.size.w + border * 2, border);
                resize(&mut buffers[2], border, rect.size.h);
                resize(&mut buffers[3], border, rect.size.h);

                resize(&mut buffers[4], size.w, rect.loc.y);
                resize(&mut buffers[5], size.w, size.h - rect.loc.y - rect.size.h);
                resize(&mut buffers[6], rect.loc.x, rect.size.h);
                resize(
                    &mut buffers[7],
                    size.w - rect.loc.x - rect.size.w,
                    rect.size.h,
                );

                locations[0] = Point::from((rect.loc.x - border, rect.loc.y - border));
                locations[1] = Point::from((rect.loc.x - border, rect.loc.y + rect.size.h));
                locations[2] = Point::from((rect.loc.x - border, rect.loc.y));
                locations[3] = Point::from((rect.loc.x + rect.size.w, rect.loc.y));

                locations[5] = Point::from((0, rect.loc.y + rect.size.h));
                locations[6] = Point::from((0, rect.loc.y));
                locations[7] = Point::from((rect.loc.x + rect.size.w, rect.loc.y));
            } else {
                buffers[0].resize((0., 0.));
                buffers[1].resize((0., 0.));
                buffers[2].resize((0., 0.));
                buffers[3].resize((0., 0.));

                buffers[4].resize(size.to_f64().to_logical(data.scale));
                buffers[5].resize((0., 0.));
                buffers[6].resize((0., 0.));
                buffers[7].resize((0., 0.));
            }
        }
    }

    /// The control panel's on-screen rect on `output`, or `None` if it has no panel.
    ///
    /// Test-only. The panel is content-sized around real captions, so its size depends on the font
    /// the machine actually has — a test cannot hardcode this rect. Measuring *inside* it is also
    /// the only way to tell the panel apart from the UI's other chrome: the four selection-border
    /// buffers alone score thousands of white pixels with the panel entirely absent. Returns `None`
    /// until the panel has drawn at least once (it is built lazily at render time).
    #[cfg(test)]
    pub fn panel_rect(&self, output: &Output) -> Option<Rectangle<i32, Physical>> {
        let Self::Open { output_data, .. } = self else {
            return None;
        };

        let output_data = output_data.get(output)?;
        let size = output_data.panel.borrow().size()?;

        Some(Rectangle::new(
            panel_location(output_data, size),
            Size::from((size.w, size.h)),
        ))
    }

    /// The panel's control geometry on `output`, panel-local and logical. Test-only, and the same
    /// value the bake and the hit test both read — a test that guessed its own coordinates would
    /// prove nothing about whether those two agree.
    #[cfg(test)]
    pub fn panel_layout(&self, output: &Output) -> Option<PanelLayout> {
        let Self::Open { output_data, .. } = self else {
            return None;
        };
        output_data.get(output)?.panel.borrow().layout
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_output(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        accent: Rgba,
        output: &Output,
        target: RenderTarget,
        push: &mut dyn FnMut(ScreenshotUiRenderElement),
    ) {
        let _span = tracy_client::span!("ScreenshotUi::render_output");

        let Self::Open {
            selection,
            output_data,
            show_pointer,
            delay,
            mode,
            capture_type,
            selected_window,
            hovered_window,
            tooltip,
            hover,
            button,
            open_anim,
            ..
        } = self
        else {
            panic!("screenshot UI must be open to render it");
        };

        let Some(output_data) = output_data.get(output) else {
            return;
        };

        let scale = output_data.scale;
        let progress = open_anim.clamped_value().clamp(0., 1.) as f32;

        // Hover only ever tracks the selection output (see the `hover` field), so a second output's
        // panel draws at rest.
        let hover = if output == &selection.0 { *hover } else { None };
        let state = PanelState {
            capture_type: *capture_type,
            window_enabled: self.window_enabled(),
            show_pointer: *show_pointer,
            delay: *delay,
            mode: *mode,
            hover,
            // `:active` only while the pointer is still on the control the press armed — a press
            // dragged off a button must let go of it, as it does anywhere else.
            active: button.on_control().filter(|c| hover == Some(*c)),
        };

        // The panel goes on top. Built lazily here (no renderer exists at open time), so this is
        // also what first populates the layout the hit test reads.
        output_data.ensure_panel(renderer, accent, state);
        if let Some(size) = output_data.panel.borrow().size() {
            let alpha = if button.is_dragging_selection() {
                0.3
            } else {
                0.9
            } * progress;
            let location = panel_location(output_data, size).to_f64().to_logical(scale);

            // Earlier-pushed elements are composited on top (the screenshot goes last), so the
            // glyphs are pushed before the panel they sit on, and the shadow after it. The tooltip
            // goes first of all: GNOME parents it to the root so it draws over the panel.
            if output == &selection.0 {
                if let Some(tip) = tooltip {
                    // Nothing until the delay elapses; then it fades in.
                    if let Some(fade) = &tip.fade {
                        let tip_alpha = fade.clamped_value().clamp(0., 1.) as f32 * progress;
                        if let Some(elem) =
                            output_data.tooltip_element(renderer, tip.control, tip_alpha)
                        {
                            push(ScreenshotUiRenderElement::Screenshot(elem));
                        }
                    }
                }
            }
            output_data.push_icons(renderer, icons, location, alpha, state, push);
            if let Some(elem) = output_data.close_element(renderer, alpha) {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
            if let Some(elem) = output_data.panel_element(renderer, location, alpha) {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
            if let Some(elem) = output_data.shadow_element(renderer, alpha) {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
        }

        if *capture_type == CaptureType::Window {
            // The selector replaces the selection chrome entirely: window thumbnails over an
            // opaque backdrop, no shade and no rectangle.
            let selected = selected_window
                .as_ref()
                .filter(|(o, _)| o == output)
                .map(|(_, id)| *id);
            let hovered = hovered_window
                .as_ref()
                .filter(|(o, _)| o == output)
                .map(|(_, id)| *id);
            output_data.push_window_selector(renderer, accent, progress, selected, hovered, push);
        } else {
            // The four corner handles ride above the border, and only on the output that owns the
            // selection — the others draw the shade alone. Screen mode has no handles: the
            // selection is the whole output, and nothing about it is draggable.
            if output == &selection.0 && *capture_type == CaptureType::Selection {
                let rect = rect_from_corner_points(selection.1, selection.2);
                output_data.push_handles(renderer, rect, progress, push);
            }

            for (buffer, loc) in zip(&output_data.buffers, &output_data.locations) {
                let elem = SolidColorRenderElement::from_buffer(
                    buffer,
                    loc.to_f64().to_logical(scale),
                    progress,
                    Kind::Unspecified,
                );
                push(elem.into());
            }
        }

        // The screenshot itself goes last — but only in Shot mode. A recording is of the *live*
        // screen, so leaving a still of the moment the picker opened underneath it would show the
        // user something other than what they are about to record (`_onCastButtonToggled` fades
        // `_stageScreenshotContainer` out for exactly this, `js/ui/screenshot.js:1888-1894`).
        if *mode == CaptureMode::Cast {
            return;
        }

        let index = match target {
            RenderTarget::Output => 0,
            RenderTarget::Screencast => 1,
            RenderTarget::ScreenCapture => 2,
        };
        let screenshot = &output_data.screenshot[index];

        if *show_pointer {
            if let Some(pointer) = screenshot.pointer_element(renderer) {
                push(ScreenshotUiRenderElement::Screenshot(pointer));
            }
        }
        if let Some(buffer) = screenshot.buffer_element(renderer) {
            push(ScreenshotUiRenderElement::Screenshot(buffer));
        }
    }

    pub fn capture_from_neutral(&self) -> Option<(Size<i32, Physical>, Vec<u8>)> {
        let _span = tracy_client::span!("ScreenshotUi::capture_from_neutral");

        let Self::Open {
            selection,
            output_data,
            show_pointer,
            capture_type,
            ..
        } = self
        else {
            panic!("screenshot UI must be open to capture");
        };

        if *capture_type == CaptureType::Window {
            return self.capture_window_from_neutral();
        }

        let data = &output_data[&selection.0];
        let OutputScreenshot {
            screen,
            pointer: screenshot_pointer,
            ..
        } = &data.screenshot[0];

        let rect = rect_from_corner_points(selection.1, selection.2);

        // The pointer neutral carries its top-left in logical output coordinates; map it back to
        // the physical space the frozen screen (and the selection rect) live in.
        let pointer = show_pointer
            .then(|| screenshot_pointer.as_ref())
            .flatten()
            .map(|(ptr, loc)| {
                let scale = Scale::from(data.scale);
                (ptr, loc.to_physical_precise_round(scale))
            });

        Some((rect.size, crop_screenshot_neutral(screen, rect, pointer)))
    }

    /// The selected window's frozen content, with the pointer composited on top if it was over
    /// that window when the picker opened.
    ///
    /// Goes through [`crop_screenshot_neutral`] like the screen path, with the whole buffer as the
    /// "crop": one composite routine, so the pointer blends the same way in both modes.
    fn capture_window_from_neutral(&self) -> Option<(Size<i32, Physical>, Vec<u8>)> {
        let Self::Open {
            output_data,
            show_pointer,
            selected_window,
            ..
        } = self
        else {
            return None;
        };

        let (output, id) = selected_window.as_ref()?;
        let data = output_data.get(output)?;
        let shot = data.windows.iter().find(|w| w.id == *id)?;

        let size = shot.neutral.size();
        let rect = Rectangle::from_size(Size::<i32, Physical>::from((size.w, size.h)));

        // The pointer neutral's location is output-local logical; the window buffer's origin is
        // the window's own. `rect` is the whole buffer, so shift the pointer into buffer space by
        // the window's on-screen origin.
        let scale = Scale::from(data.scale);
        let pointer = show_pointer
            .then(|| data.screenshot[0].pointer.as_ref())
            .flatten()
            .map(|(ptr, loc)| {
                let loc = (*loc - shot.rect.loc).to_physical_precise_round(scale);
                (ptr, loc)
            });

        Some((
            rect.size,
            crop_screenshot_neutral(&shot.neutral, rect, pointer),
        ))
    }

    pub fn action(&self, raw: Keysym, mods: ModifiersState) -> Option<Action> {
        let Self::Open { button, .. } = self else {
            return None;
        };

        // Pressing Space while the button is down goes into origin moving rather than capture.
        if matches!(button, Button::Down { .. }) && raw == Keysym::space {
            return None;
        }

        action(raw, mods)
    }

    /// The current selection as a rectangle in **global logical** coordinates.
    ///
    /// This is what `org.gnome.Shell.Screenshot.SelectArea` hands back, so it has to be in the same
    /// space its caller then passes to `ScreenshotArea` — output-local physical is what the UI
    /// works in and is not it.
    pub fn selection_rect_global(&self) -> Option<Rectangle<i32, Logical>> {
        let Self::Open {
            selection,
            output_data,
            ..
        } = self
        else {
            return None;
        };

        let (output, a, b) = selection;
        let rect = rect_from_corner_points(*a, *b);
        let scale = output_data.get(output)?.scale;
        let logical = rect.to_f64().to_logical(scale);
        let loc = output.current_location();

        Some(Rectangle::new(
            Point::from((
                loc.x + logical.loc.x.round() as i32,
                loc.y + logical.loc.y.round() as i32,
            )),
            Size::from((logical.size.w.round() as i32, logical.size.h.round() as i32)),
        ))
    }

    pub fn selection_output(&self) -> Option<&Output> {
        if let Self::Open {
            selection: (output, _, _),
            ..
        } = self
        {
            Some(output)
        } else {
            None
        }
    }

    pub fn output_size(&self, output: &Output) -> Option<(Size<i32, Physical>, f64, Transform)> {
        if let Self::Open { output_data, .. } = self {
            let data = output_data.get(output)?;
            Some((data.size, data.scale, data.transform))
        } else {
            None
        }
    }

    /// The pointer has moved to `point` relative to the current selection output.
    ///
    /// The point may be outside output bounds. Returns whether anything on screen changed.
    pub fn pointer_motion(&mut self, point: Point<i32, Physical>, slot: Option<TouchSlot>) -> bool {
        // Hover first: it tracks even with no button down, which is the whole point of it.
        let hover_changed = self.update_hover(point);

        let Self::Open {
            selection,
            output_data,
            capture_type,
            cursor,
            button:
                Button::Down {
                    touch_slot,
                    on_control,
                    last_pos,
                    grab,
                },
            ..
        } = self
        else {
            return hover_changed;
        };

        if *touch_slot != slot {
            return hover_changed;
        }

        let previous = last_pos.1;
        last_pos.1 = point;

        // A press that landed on a control drags nothing, and Screen mode has no selection to drag.
        if on_control.is_some() || *capture_type != CaptureType::Selection {
            return hover_changed;
        }

        let size = output_data[&selection.0].size;
        match grab {
            Grab::New => {
                selection.2 =
                    Point::new(point.x.clamp(0, size.w - 1), point.y.clamp(0, size.h - 1));
            }
            Grab::Move(state) => {
                // The offset model *is* GNOME's rubberbanding (`:610-640`): it tracks the pointer's
                // absolute position, so pushing into an edge clamps and coming back off it moves
                // again immediately, which is what its overshoot bookkeeping buys with deltas.
                let delta = point - (selection.1 + state.pointer_offset);

                let desired = rect_from_corner_points(selection.1 + delta, selection.2 + delta);
                let bounds = Rectangle::from_size(size - desired.size);
                let clamped_loc = desired.loc.constrain(bounds);

                let delta = clamped_loc - rect_from_corner_points(selection.1, selection.2).loc;
                selection.1 += delta;
                selection.2 += delta;
            }
            Grab::Resize { x, y } => {
                // Only the grabbed axes move: a pure edge drag pins the other one (`:645-650`).
                // Deltas rather than the absolute position, because `selection.2` is the moving
                // corner and the pointer sits on it only after the press-time warp.
                let mut delta = point - previous;
                if x.is_none() {
                    delta.x = 0;
                }
                if y.is_none() {
                    delta.y = 0;
                }
                selection.2 = Point::new(
                    (selection.2.x + delta.x).clamp(0, size.w - 1),
                    (selection.2.y + delta.y).clamp(0, size.h - 1),
                );
            }
        }

        // After the drag, not before: `update_hover` ran at the top against the geometry this
        // motion was about to change, so a handle dragged past the far side would keep advertising
        // the corner it used to be for one motion longer.
        *cursor = grab.cursor(selection.1, selection.2);

        self.update_buffers();
        true
    }

    /// Recompute which control the pointer is over, on the selection output. Returns whether it
    /// changed — a rebake and a redraw only happen when it does.
    fn update_hover(&mut self, point: Point<i32, Physical>) -> bool {
        let window_enabled = self.window_enabled();
        let Self::Open {
            selection,
            output_data,
            capture_type,
            hover,
            hovered_window,
            cursor,
            button,
            ..
        } = self
        else {
            return false;
        };

        let data = output_data.get(&selection.0);

        // The crosshair belongs to the area selector, not to the whole picker: in GNOME it is set
        // on `_areaSelector` (`set_cursor_type`, `js/ui/screenshot.js:448`), so the panel's buttons
        // are siblings that inherit the default, and leaving Selection mode resets it outright
        // (`:1792`). A crosshair over a button says "click to select an area" about a surface that
        // does nothing of the kind.
        let new_cursor = match button {
            // A held drag keeps its own cursor wherever the pointer wanders, including over the
            // panel: GNOME holds a stage grab for the duration, so nothing else can claim it.
            Button::Down {
                on_control: None,
                grab,
                ..
            } if *capture_type == CaptureType::Selection => grab.cursor(selection.1, selection.2),
            _ if *capture_type != CaptureType::Selection
                || data.is_some_and(|data| data.over_chrome(point)) =>
            {
                CursorIcon::Default
            }
            // Free pointer over the selectable area: whatever a press here would grab.
            _ => data.map_or(CursorIcon::Crosshair, |data| {
                area_target(
                    rect_from_corner_points(selection.1, selection.2),
                    point,
                    data.scale,
                )
                .cursor()
            }),
        };
        let cursor_changed = *cursor != new_cursor;
        *cursor = new_cursor;
        // An insensitive control takes no hover, exactly as `reactive = false` gives St.Button no
        // `notify::hover` — which is what keeps it from lighting up, and from offering a tooltip
        // for something it will not do.
        let new = data
            .and_then(|data| data.control_at(point))
            .filter(|c| *c != Control::Type(CaptureType::Window) || window_enabled);
        // The panel sits above the selector, so a control under the pointer wins the hover — the
        // window behind it must not light up too.
        let new_window = (*capture_type == CaptureType::Window && new.is_none())
            .then(|| data.and_then(|data| data.window_at(point)))
            .flatten()
            .map(|id| (selection.0.clone(), id));

        if *hover == new && *hovered_window == new_window && !cursor_changed {
            return false;
        }
        *hover = new;
        *hovered_window = new_window;
        self.retarget_tooltip();
        true
    }

    /// Point the tooltip at whatever the pointer is now on, restarting its delay.
    ///
    /// Moving between controls restarts the wait rather than carrying the old tip across: GNOME's
    /// `close()` cancels a pending timeout outright and the next `open()` schedules a fresh one
    /// (`js/ui/screenshot.js:122-129`).
    fn retarget_tooltip(&mut self) {
        let Self::Open {
            hover,
            tooltip,
            clock,
            ..
        } = self
        else {
            return;
        };

        match *hover {
            Some(control) if control.tooltip().is_some() => {
                if tooltip.as_ref().map(|t| t.control) == Some(control) {
                    return;
                }
                *tooltip = Some(TooltipState {
                    control,
                    due: clock.now_unadjusted() + TOOLTIP_DELAY,
                    fade: None,
                });
            }
            _ => *tooltip = None,
        }
    }

    pub fn pointer_down(
        &mut self,
        output: Output,
        point: Point<i32, Physical>,
        slot: Option<TouchSlot>,
        move_existing: bool,
    ) -> Option<PointerDown> {
        let Self::Open {
            selection,
            output_data,
            capture_type,
            selected_window,
            button,
            cursor,
            ..
        } = self
        else {
            return None;
        };

        // Check if this is a second touch (different slot) while already dragging.
        if let Some(new_slot) = slot {
            if let Button::Down {
                on_control: None,
                grab,
                last_pos,
                ..
            } = button
            {
                if matches!(grab, Grab::New) {
                    *grab = Grab::Move(MoveState {
                        pointer_offset: last_pos.1 - selection.1,
                        touch_slot: Some(new_slot),
                    });
                }
            }
        }

        if button.is_down() {
            return None;
        }

        if move_existing {
            if output != selection.0 || *capture_type != CaptureType::Selection {
                return None;
            }

            *button = Button::Down {
                touch_slot: slot,
                on_control: None,
                last_pos: (output, point),
                grab: Grab::Move(MoveState {
                    pointer_offset: point - selection.1,
                    touch_slot: slot,
                }),
            };
            return Some(PointerDown::Redraw);
        }

        let output_data = output_data.get(&output)?;

        if let Some(control) = output_data.control_at(point) {
            *button = Button::Down {
                touch_slot: slot,
                on_control: Some(control),
                last_pos: (output, point),
                grab: Grab::New,
            };
            // A control lights up while held, so the caller still owes a redraw.
            return Some(PointerDown::Redraw);
        }

        // In Window mode a press outside the panel is a window pick, not a drag.
        if *capture_type == CaptureType::Window {
            let picked = output_data.window_at(point);
            *button = Button::Down {
                touch_slot: slot,
                on_control: None,
                last_pos: (output.clone(), point),
                grab: Grab::New,
            };
            if let Some(id) = picked {
                // GNOME checks on release, but it also unchecks every other window at once — the
                // selection is single-valued, so assigning it is the whole operation
                // (`screenshot.js:1643-1658`).
                *selected_window = Some((output, id));
                return Some(PointerDown::Redraw);
            }
            return None;
        }

        // In Screen mode the selection is the output; a press on the frozen screen must not start
        // dragging a rectangle out of it.
        if *capture_type != CaptureType::Selection {
            *button = Button::Down {
                touch_slot: slot,
                on_control: None,
                last_pos: (output, point),
                grab: Grab::New,
            };
            return None;
        }

        // What the press grabbed. Only a press on the *selection output's* own rectangle can grab
        // it — a second monitor has its own panel but not this selection.
        let target = if output == selection.0 {
            area_target(
                rect_from_corner_points(selection.1, selection.2),
                point,
                output_data.scale,
            )
        } else {
            AreaTarget::Outside
        };

        let size = output_data.size;
        let clamp = |p: Point<i32, Physical>| {
            Point::new(p.x.clamp(0, size.w - 1), p.y.clamp(0, size.h - 1))
        };

        let (grab, warp) = match target {
            AreaTarget::Outside => {
                let point = clamp(point);
                *selection = (output.clone(), point, point);
                (Grab::New, None)
            }
            AreaTarget::Move => (
                Grab::Move(MoveState {
                    pointer_offset: point - rect_from_corner_points(selection.1, selection.2).loc,
                    touch_slot: slot,
                }),
                None,
            ),
            AreaTarget::Resize { x, y } => {
                // `selection.1` becomes the stationary corner and `selection.2` the moving one, so
                // the rest of the drag is "move `.2`" no matter which handle was taken
                // (`js/ui/screenshot.js:524-537`).
                let rect = rect_from_corner_points(selection.1, selection.2);
                let (left, top) = (rect.loc.x, rect.loc.y);
                let (right, bottom) = (left + rect.size.w - 1, top + rect.size.h - 1);

                let (stay_x, move_x) = match x {
                    Some(Side::Low) => (right, left),
                    Some(Side::High) => (left, right),
                    None => (selection.1.x, selection.2.x),
                };
                let (stay_y, move_y) = match y {
                    Some(Side::Low) => (bottom, top),
                    Some(Side::High) => (top, bottom),
                    None => (selection.1.y, selection.2.y),
                };
                *selection = (
                    output.clone(),
                    Point::new(stay_x, stay_y),
                    Point::new(move_x, move_y),
                );

                // Snap the pointer onto the side it grabbed; an edge is grabbable from outside it,
                // so without this the drag would run from beside the thing it moves.
                let warp = Point::new(
                    if x.is_some() { move_x } else { point.x },
                    if y.is_some() { move_y } else { point.y },
                );
                (Grab::Resize { x, y }, Some(warp))
            }
        };

        // GNOME sets the cursor inside `_onPress` (`js/ui/screenshot.js:465`, `:519`), and it has
        // to: taking hold of a handle is a cursor change with no motion behind it.
        *cursor = grab.cursor(selection.1, selection.2);

        *button = Button::Down {
            touch_slot: slot,
            on_control: None,
            // The warp lands here next, and a resize measures deltas from it.
            last_pos: (output, warp.unwrap_or(point)),
            grab,
        };

        self.update_buffers();

        Some(warp.map_or(PointerDown::Redraw, PointerDown::WarpTo))
    }

    pub fn pointer_up(&mut self, slot: Option<TouchSlot>) -> Option<PointerUp> {
        let Self::Open {
            selection,
            output_data,
            button,
            ..
        } = self
        else {
            return None;
        };

        let Button::Down {
            touch_slot,
            on_control,
            ref last_pos,
            ref mut grab,
            ..
        } = *button
        else {
            return None;
        };

        if touch_slot != slot {
            // This is not our main touch, but it might be the move touch. If so, stop the move.
            if let Some(state) = grab.move_state() {
                if state.touch_slot.is_some_and(|m_slot| Some(m_slot) == slot) {
                    *grab = Grab::New;
                }
            }

            return None;
        }

        let last_pos = last_pos.clone();
        *button = Button::Up;

        // A press armed on a control acts only if the release lands on that same control.
        if let Some(control) = on_control {
            let (output, point) = last_pos;
            let released_on = output_data.get(&output).and_then(|d| d.control_at(point));
            if released_on != Some(control) {
                return Some(PointerUp::Redraw);
            }
            return Some(self.activate(control));
        }

        // Check if the resulting selection is zero-sized, and try to come up with a small
        // default rectangle.
        let (output, a, b) = selection;
        let mut rect = rect_from_corner_points(*a, *b);
        if rect.size.is_empty() || rect.size == Size::from((1, 1)) {
            let data = &output_data[output];
            rect = Rectangle::new(
                Point::from((rect.loc.x - 16, rect.loc.y - 16)),
                Size::from((32, 32)),
            )
            .intersection(Rectangle::from_size(data.size))
            .unwrap_or_default();
            *a = rect.loc;
            *b = rect.loc + rect.size - Size::from((1, 1));
        }

        self.update_buffers();

        // "We might have finished creating a new selection, so we need to update the cursor"
        // (`_onRelease`, `js/ui/screenshot.js:578-581`): the rectangle under the pointer is not the
        // one that was there when the drag began, so what a press would grab has changed.
        self.update_hover(last_pos.1);

        Some(PointerUp::Redraw)
    }

    /// Act on a clicked control. Everything the panel owns it settles here; only the two that need
    /// the compositor (firing the shutter, closing the UI) travel back out.
    fn activate(&mut self, control: Control) -> PointerUp {
        match control {
            Control::Capture => PointerUp::Capture,
            Control::Close => PointerUp::Close,
            Control::Type(ty) => {
                self.set_capture_type(ty);
                PointerUp::Redraw
            }
            Control::ShowPointer => {
                self.toggle_pointer();
                PointerUp::Redraw
            }
            Control::Delay => {
                self.cycle_delay();
                PointerUp::Redraw
            }
            Control::ShotCast(i) => {
                self.set_mode(CaptureMode::from_index(i));
                PointerUp::Redraw
            }
        }
    }
}

impl OutputScreenshot {
    /// The frozen screen as renderer-neutral captures (a Vulkan session).
    pub fn from_neutrals(
        screen: MemoryBuffer,
        pointer: Option<(MemoryBuffer, Point<f64, Logical>)>,
    ) -> Self {
        Self {
            screen,
            pointer,
            screen_vk: RefCell::new(None),
            pointer_vk: RefCell::new(None),
        }
    }

    /// The frozen-screen element, or `None` if it can't be drawn — a failed Vulkan upload. Callers
    /// must skip it rather than substitute anything: there is no second copy of the frozen screen.
    fn buffer_element(
        &self,
        renderer: &mut VulkanRenderer,
    ) -> Option<CapturedTextureRenderElement> {
        let Self {
            screen, screen_vk, ..
        } = self;

        let vk = &mut *renderer;
        let tb = upload_cached(vk, screen, screen_vk)?;
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                tb,
                (0., 0.),
                1.,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }

    /// The composited-pointer element. `None` when no pointer was captured, or when it can't be
    /// drawn (see [`Self::buffer_element`]).
    fn pointer_element(
        &self,
        renderer: &mut VulkanRenderer,
    ) -> Option<CapturedTextureRenderElement> {
        let Self {
            pointer,
            pointer_vk,
            ..
        } = self;

        let vk = &mut *renderer;
        let (neutral, location) = pointer.as_ref()?;
        let tb = upload_cached(vk, neutral, pointer_vk)?;
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                tb,
                *location,
                1.,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }
}

impl OutputData {
    /// Build the panel (and its shadow and close button) into `VkTexture`s if missing or stale —
    /// a [`PanelState`], scale or renderer-context change. Failures leave the texture `None`, so
    /// the panel just doesn't draw.
    fn ensure_panel(&self, renderer: &mut VulkanRenderer, accent: Rgba, state: PanelState) {
        let scale = self.scale;
        let context = renderer.context_id();
        {
            let cache = self.panel.borrow();
            if cache.texture.is_some()
                && cache.scale == scale
                && cache.context.as_ref() == Some(&context)
                && cache.state == Some(state)
            {
                return;
            }
        }

        let built = generate_panel(renderer, scale, accent, state)
            .map_err(|err| warn!("error rendering the screenshot panel: {err:?}"))
            .ok();
        let (texture, layout) = match built {
            Some((texture, layout)) => (Some(texture), Some(layout)),
            None => (None, None),
        };
        let shadow = texture
            .as_ref()
            .map(|t| Size::<i32, Physical>::from((t.width() as i32, t.height() as i32)))
            .and_then(|panel_size| {
                generate_panel_shadow(renderer, scale, panel_size)
                    .map_err(|err| warn!("error rendering the screenshot panel's shadow: {err:?}"))
                    .ok()
            });
        let close = generate_close_button(renderer, scale, state.hover == Some(Control::Close))
            .map_err(|err| warn!("error rendering the screenshot close button: {err:?}"))
            .ok();
        let handle = generate_handle(renderer, scale)
            .map_err(|err| warn!("error rendering a selection handle: {err:?}"))
            .ok();

        *self.panel.borrow_mut() = PanelCache {
            scale,
            context: Some(context),
            state: Some(state),
            texture,
            layout,
            shadow,
            close,
            handle,
        };
    }

    /// Whether an output-local **physical** point is over the panel's own chrome — the card or the
    /// close button that straddles its corner.
    ///
    /// Broader than [`Self::control_at`] on purpose: the card's background between buttons is
    /// chrome too, and it is not a surface you can start a selection on.
    fn over_chrome(&self, point: Point<i32, Physical>) -> bool {
        let Some(panel) = self.panel_rect_logical() else {
            return false;
        };
        let p = point.to_f64().to_logical(self.scale);
        let close = widget::IconButton::new(close_rect(panel), CLOSE_ICON_PX, style::TRANSPARENT);
        panel.contains(p) || close.contains(p)
    }

    /// The window whose selector slot contains an output-local **physical** point.
    ///
    /// Slots do not overlap (the layout packs them into rows), so first-hit is unambiguous. Tested
    /// against the slot rather than the thumbnail: a window with a large shadow margin draws
    /// smaller than its slot, and GNOME's button — which is what is reactive — is the slot.
    fn window_at(&self, point: Point<i32, Physical>) -> Option<u64> {
        let p = point.to_f64().to_logical(self.scale);
        zip(&self.windows, &self.slots)
            .find(|(_, slot)| slot.contains(p))
            .map(|(shot, _)| shot.id)
    }

    /// The panel's on-screen rect in output-logical px, or `None` before the first bake.
    fn panel_rect_logical(&self) -> Option<Rectangle<f64, Logical>> {
        let cache = self.panel.borrow();
        let layout = cache.layout?;
        let size = cache.size()?;
        let loc = panel_location(self, size).to_f64().to_logical(self.scale);
        Some(Rectangle::new(loc, layout.size))
    }

    /// The control at an output-local **physical** point, or `None`.
    ///
    /// Everything but the close button lives inside the panel, so this is one coordinate shift plus
    /// [`PanelLayout::control_at`]; the close button is a sibling and is tested against its own
    /// circle.
    fn control_at(&self, point: Point<i32, Physical>) -> Option<Control> {
        let panel = self.panel_rect_logical()?;
        let p = point.to_f64().to_logical(self.scale);

        let close = widget::IconButton::new(close_rect(panel), CLOSE_ICON_PX, style::TRANSPARENT);
        if close.contains(p) {
            return Some(Control::Close);
        }

        let layout = self.panel.borrow().layout?;
        layout.control_at(p - panel.loc)
    }

    /// The panel's drop-shadow element, placed so its baked panel-footprint aligns with the panel
    /// on screen (the buffer pads the footprint by the blur bleed, so the element sits up-left of
    /// the panel by that margin). Composited *behind* the panel. `None` until the shadow is baked.
    fn shadow_element(
        &self,
        renderer: &mut VulkanRenderer,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let scale = self.scale;
        let (texture, panel_size) = {
            let cache = self.panel.borrow();
            (cache.shadow.clone()?, cache.size()?)
        };
        let (margin, _) = shadow_pad(scale);
        let loc = (panel_location(self, panel_size) - Point::from((margin, margin)))
            .to_f64()
            .to_logical(scale);
        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                buffer,
                loc,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }

    /// The tooltip pill for `control`, above it and centred on it.
    ///
    /// GNOME adds tooltips to the **root** widget rather than to the buttons they describe
    /// (`js/ui/screenshot.js:1314`), which is what lets them draw over the panel — so this is
    /// pushed before the panel, not with it.
    fn tooltip_element(
        &self,
        renderer: &mut VulkanRenderer,
        control: Control,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let text = control.tooltip()?;
        let anchor = self.control_rect(control)?;
        let scale = self.scale;
        let size = widget::Tooltip::size(text);

        // Centre on the control, then clamp into the output — GNOME clamps to the stage for the
        // same reason: a tip on the leftmost control would otherwise hang off the screen.
        let output = self.size.to_f64().to_logical(scale);
        let x =
            (anchor.loc.x + (anchor.size.w - size.w) / 2.).clamp(0., (output.w - size.w).max(0.));
        let y = anchor.loc.y - size.h - TOOLTIP_Y_OFFSET;

        let revision = widget::Revision::new().of(text).done();
        let texture = widget::bake(
            renderer,
            &mut self.tooltip_cache.borrow_mut(),
            scale,
            size,
            revision,
            |vk| TextShaper::new(vk, scale).shape(text, TextStyle::new(widget::Tooltip::TEXT_PT)),
            |frame, phys, label| {
                let mut p = Painter::new(frame, scale, phys);
                p.tooltip(size, label)
            },
        )
        .map_err(|err| warn!("error rendering the screenshot tooltip: {err:?}"))
        .ok()?;

        Some(self.texture_element(renderer, texture, Point::from((x, y)), alpha))
    }

    /// A control's on-screen rect in output-logical px. `None` before the panel has baked.
    fn control_rect(&self, control: Control) -> Option<Rectangle<f64, Logical>> {
        let panel = self.panel_rect_logical()?;
        if control == Control::Close {
            return Some(close_rect(panel));
        }

        let layout = self.panel.borrow().layout?;
        let local = match control {
            Control::Type(ty) => {
                layout.type_buttons[CaptureType::ROW.iter().position(|t| *t == ty)?]
            }
            Control::ShotCast(i) => widget::Segmented::segment_rect(layout.shot_cast, i),
            Control::Capture => layout.capture,
            Control::ShowPointer => layout.show_pointer,
            Control::Delay => layout.delay,
            Control::Close => unreachable!("handled above"),
        };
        Some(Rectangle::new(panel.loc + local.loc, local.size))
    }

    /// The Window-mode selector: an opaque backdrop, then every frozen window at its slot with a
    /// border that tints on hover and on selection.
    ///
    /// Elements are pushed front-to-back (the first pushed composites on top), so each window's
    /// border goes before its thumbnail and the backdrop goes last.
    fn push_window_selector(
        &self,
        renderer: &mut VulkanRenderer,
        accent: Rgba,
        alpha: f32,
        selected: Option<u64>,
        hovered: Option<u64>,
        push: &mut dyn FnMut(ScreenshotUiRenderElement),
    ) {
        for (shot, slot) in zip(&self.windows, &self.slots) {
            // The thumbnail keeps the window's aspect: `compute_slots` already sized the slot from
            // that rect, so filling the slot is right — but a zero-sized window would divide by
            // zero on the way there.
            if slot.size.w <= 0. || slot.size.h <= 0. {
                continue;
            }

            if let Some(elem) = shot.element(renderer, *slot, alpha) {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }

            let state = if selected == Some(shot.id) {
                SelectorBorder::Checked
            } else if hovered == Some(shot.id) {
                SelectorBorder::Hovered
            } else {
                continue;
            };
            if let Some(elem) = self.selector_border_element(renderer, *slot, state, accent, alpha)
            {
                push(ScreenshotUiRenderElement::Screenshot(elem));
            }
        }

        // The backdrop, over the frozen screen and under everything above.
        let size = self.size.to_f64().to_logical(self.scale);
        let buffer = SolidColorBuffer::new(size, SELECTOR_BG);
        push(
            SolidColorRenderElement::from_buffer(&buffer, (0., 0.), alpha, Kind::Unspecified)
                .into(),
        );
    }

    /// One window's selection border, baked at the slot's size. Not cached: a slot size is
    /// per-window and only changes when the picker reopens, and the two tints differ per window.
    fn selector_border_element(
        &self,
        renderer: &mut VulkanRenderer,
        slot: Rectangle<f64, Logical>,
        state: SelectorBorder,
        accent: Rgba,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let scale = self.scale;
        // The border sits *outside* the thumbnail, as GNOME's does (`vfunc_allocate` grows the
        // border box by the border width on every side, `js/ui/screenshot.js:902-909`).
        let outer = Rectangle::new(
            slot.loc - Point::from((SELECTOR_BORDER, SELECTOR_BORDER)),
            Size::from((
                slot.size.w + SELECTOR_BORDER * 2.,
                slot.size.h + SELECTOR_BORDER * 2.,
            )),
        );
        let size = widget::physical_size(scale, outer.size);
        let texture = widget::bake_uncached_sized(renderer, size, |frame| {
            let mut p = Painter::new(frame, scale, size);
            p.clear(style::TRANSPARENT)?;
            let (border, fill) = state.colors(accent);
            if let Some(fill) = fill {
                p.fill_rounded_full(SELECTOR_RADIUS + SELECTOR_BORDER, fill)?;
            }
            p.stroke_rounded_full(SELECTOR_RADIUS + SELECTOR_BORDER, SELECTOR_BORDER, border)?;
            Ok(())
        })
        .map_err(|err| warn!("error rendering the window selector border: {err:?}"))
        .ok()?;

        Some(self.texture_element(renderer, texture, outer.loc, alpha))
    }

    /// The panel element, or `None` if it hasn't been built (see [`Self::ensure_panel`]).
    fn panel_element(
        &self,
        renderer: &mut VulkanRenderer,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let texture = self.panel.borrow().texture.clone()?;
        Some(self.texture_element(renderer, texture, location, alpha))
    }

    /// The close-button element, straddling the panel's top corner.
    fn close_element(
        &self,
        renderer: &mut VulkanRenderer,
        alpha: f32,
    ) -> Option<CapturedTextureRenderElement> {
        let texture = self.panel.borrow().close.clone()?;
        let loc = close_rect(self.panel_rect_logical()?).loc;
        Some(self.texture_element(renderer, texture, loc, alpha))
    }

    fn texture_element(
        &self,
        renderer: &mut VulkanRenderer,
        texture: VkTexture,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> CapturedTextureRenderElement {
        let buffer = TextureBuffer::from_texture(
            renderer,
            texture,
            self.scale,
            Transform::Normal,
            Vec::new(),
        );
        CapturedTextureRenderElement(TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            alpha,
            None,
            None,
            Kind::Unspecified,
        ))
    }

    /// The four selection corner handles, centred on the corners of `rect` (output-local physical).
    ///
    /// `_updateSelectionRect` places each one at `corner - handleSize / 2`
    /// (`js/ui/screenshot.js:346-352`), so they straddle the corner rather than sit inside it —
    /// which is also why the hit test in [`area_target`] reaches outside the rectangle.
    fn push_handles(
        &self,
        renderer: &mut VulkanRenderer,
        rect: Rectangle<i32, Physical>,
        alpha: f32,
        push: &mut dyn FnMut(ScreenshotUiRenderElement),
    ) {
        let Some(texture) = self.panel.borrow().handle.clone() else {
            return;
        };
        let (side, margin) = handle_pad(self.scale);
        // The texture is the circle plus its shadow margin, so the top-left of the *circle* is
        // `margin` in from the buffer's own top-left.
        let offset = f64::from(side) / 2. + f64::from(margin);

        let (left, top) = (rect.loc.x, rect.loc.y);
        let (right, bottom) = (left + rect.size.w - 1, top + rect.size.h - 1);
        for (cx, cy) in [(left, top), (right, top), (left, bottom), (right, bottom)] {
            let loc =
                Point::<f64, Physical>::from((f64::from(cx) - offset, f64::from(cy) - offset))
                    .to_logical(self.scale);
            let elem = self.texture_element(renderer, texture.clone(), loc, alpha);
            push(ScreenshotUiRenderElement::Screenshot(elem));
        }
    }

    /// Composite every glyph on the panel.
    ///
    /// Icons are render elements, not paint verbs, so they cannot go into the bake — they ride on
    /// top of it and must fade with it, hence `alpha` rather than `1.`. `origin` is the panel's
    /// on-screen top-left; the layout's centres are panel-local.
    fn push_icons(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        origin: Point<f64, Logical>,
        alpha: f32,
        state: PanelState,
        push: &mut dyn FnMut(ScreenshotUiRenderElement),
    ) {
        let Some(layout) = self.panel.borrow().layout else {
            return;
        };
        let scale = self.scale;

        let mut icon = |name: &str, px: f64, color: Rgba, origin, center| {
            if let Some(elem) = widget::icon_element_alpha(
                renderer,
                icons,
                &[name],
                px,
                scale,
                color,
                origin,
                center,
                alpha,
            ) {
                push(ScreenshotUiRenderElement::Screenshot(
                    CapturedTextureRenderElement(elem),
                ));
            }
        };

        for (i, ty) in CaptureType::ROW.iter().enumerate() {
            let button = widget::IconLabelButton::new(layout.type_buttons[i]);
            icon(
                ty.icon(),
                widget::IconLabelButton::ICON_PX,
                type_fg(state.enables(*ty)),
                origin,
                button.icon_centre(),
            );
        }

        // The checked segment is a solid white pill, so its glyph inverts to the panel colour
        // (`_screenshot.scss:108`); the other keeps `$osd_fg_color`.
        for mode in [CaptureMode::Shot, CaptureMode::Cast] {
            let seg = widget::Segmented::segment_rect(layout.shot_cast, mode.index());
            let color = if state.mode == mode {
                style::OSD_BG
            } else {
                style::OSD_FG
            };
            icon(
                mode.icon(),
                widget::Segmented::ICON_PX,
                color,
                origin,
                centre(seg),
            );
        }

        icon(
            "screenshot-ui-show-pointer-symbolic",
            widget::IconButton::ICON_PX,
            style::OSD_FG,
            origin,
            centre(layout.show_pointer),
        );

        // Armed, the button carries its own baked number instead — no glyph.
        if state.delay == 0 {
            icon(
                "alarm-symbolic",
                widget::IconButton::ICON_PX,
                style::OSD_FG,
                origin,
                centre(layout.delay),
            );
        }

        if let Some(panel) = self.panel_rect_logical() {
            let close = close_rect(panel);
            icon(
                "preview-close-symbolic",
                CLOSE_ICON_PX,
                style::TEXT,
                close.loc,
                centre(close) - close.loc,
            );
        }
    }
}

/// Upload a neutral CPU buffer to a `VkTexture` once, caching it for reuse across frames (a
/// full-screen re-upload every frame churns virtio-gpu blobs).
fn upload_cached(
    vk: &mut VulkanRenderer,
    neutral: &MemoryBuffer,
    cache: &VkCache,
) -> Option<TextureBuffer<VkTexture>> {
    if cache.borrow().is_none() {
        match TextureBuffer::from_memory_buffer(vk, neutral) {
            Ok(tb) => *cache.borrow_mut() = Some(tb),
            Err(err) => warn!("error uploading screenshot overlay to Vulkan: {err:?}"),
        }
    }
    cache.borrow().clone()
}

fn action(raw: Keysym, mods: ModifiersState) -> Option<Action> {
    if raw == Keysym::Escape {
        return Some(Action::CancelScreenshot);
    }

    if mods.alt || mods.shift {
        return None;
    }

    if !mods.ctrl && (raw == Keysym::space || raw == Keysym::Return) {
        return Some(Action::ConfirmScreenshot {
            write_to_disk: true,
        });
    }
    if mods.ctrl && raw == Keysym::c {
        return Some(Action::ConfirmScreenshot {
            write_to_disk: false,
        });
    }

    if mods.ctrl {
        return None;
    }

    // The type row and the shot/cast pill answer to single keys, as in `screenshot.js:2207-2233`.
    // An insensitive control still swallows its key there, and `set_capture_type` refuses the
    // switch on its own, so these need no extra gating here.
    match raw {
        Keysym::p => Some(Action::ScreenshotTogglePointer),
        Keysym::s => Some(Action::ScreenshotTypeSelection),
        Keysym::c => Some(Action::ScreenshotTypeScreen),
        Keysym::w => Some(Action::ScreenshotTypeWindow),
        Keysym::v => Some(Action::ScreenshotToggleCast),
        _ => None,
    }
}

pub fn rect_from_corner_points(
    a: Point<i32, Physical>,
    b: Point<i32, Physical>,
) -> Rectangle<i32, Physical> {
    let x1 = min(a.x, b.x);
    let y1 = min(a.y, b.y);
    let x2 = max(a.x, b.x);
    let y2 = max(a.y, b.y);
    // We're adding + 1 because the pointer is clamped to output size - 1, so to get the full
    // screen worth of selection we must add back that + 1.
    Rectangle::from_extremities((x1, y1), (x2 + 1, y2 + 1))
}

/// Copy an out-of-bounds-tolerant `rect` out of a tightly packed RGBA buffer. Anything outside
/// `size` comes back transparent rather than clamped or wrapped.
pub(crate) fn crop_rgba(
    size: Size<i32, Buffer>,
    src: &[u8],
    rect: Rectangle<i32, Physical>,
) -> Vec<u8> {
    let (fw, fh) = (size.w, size.h);
    let (rw, rh) = (rect.size.w.max(0), rect.size.h.max(0));
    let mut out = vec![0u8; (rw * rh * 4) as usize];

    // Copy the in-bounds horizontal span of each row wholesale.
    for y in 0..rh {
        let sy = rect.loc.y + y;
        if sy < 0 || sy >= fh {
            continue;
        }
        let sx0 = rect.loc.x.max(0);
        let sx1 = (rect.loc.x + rw).min(fw);
        if sx1 <= sx0 {
            continue;
        }
        let src_off = ((sy * fw + sx0) * 4) as usize;
        let dst_off = ((y * rw + (sx0 - rect.loc.x)) * 4) as usize;
        let n = ((sx1 - sx0) * 4) as usize;
        out[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
    }

    out
}

/// Crop `rect` out of the frozen-screen `neutral` buffer (tightly-packed `Abgr8888`, origin at the
/// output's top-left) and, if `pointer` is given, composite that premultiplied buffer on top at its
/// physical origin. Pure CPU — the owned-Vulkan save-to-disk path, so it never reads back through
/// GLES. Returns `rect.size.w * rect.size.h * 4` bytes; out-of-bounds source pixels stay zero.
pub(crate) fn crop_screenshot_neutral(
    neutral: &MemoryBuffer,
    rect: Rectangle<i32, Physical>,
    pointer: Option<(&MemoryBuffer, Point<i32, Physical>)>,
) -> Vec<u8> {
    let mut out = crop_rgba(neutral.size(), neutral.data(), rect);
    let (rw, rh) = (rect.size.w.max(0), rect.size.h.max(0));

    // Composite the pointer on top: premultiplied `Abgr8888` "over" (out = src + dst·(255−a)/255).
    if let Some((ptr, ptr_origin)) = pointer {
        let (pw, ph) = (ptr.size().w, ptr.size().h);
        let pdata = ptr.data();
        for py in 0..ph {
            let cy = ptr_origin.y - rect.loc.y + py;
            if cy < 0 || cy >= rh {
                continue;
            }
            for px in 0..pw {
                let cx = ptr_origin.x - rect.loc.x + px;
                if cx < 0 || cx >= rw {
                    continue;
                }
                let si = ((py * pw + px) * 4) as usize;
                let a = pdata[si + 3] as u32;
                if a == 0 {
                    continue;
                }
                let inv = 255 - a;
                let di = ((cy * rw + cx) * 4) as usize;
                for c in 0..4 {
                    let d = out[di + c] as u32;
                    let v = pdata[si + c] as u32 + (d * inv + 127) / 255;
                    out[di + c] = v.min(255) as u8;
                }
            }
        }
    }

    out
}

/// What the capture button will act on — GNOME's three type buttons
/// (`screenshot.js:1305-1348`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CaptureType {
    /// A dragged rectangle. GNOME's `Selection`, and the only mode niri's picker ever had.
    #[default]
    Selection,
    /// The whole output.
    Screen,
    /// A window chosen from the selector. Not yet implemented — see slice 2 in
    /// `docs/fork/screenshot-ui-port.md`; the button is built but hidden until it works.
    Window,
}

impl CaptureType {
    /// The type row, left to right, in GNOME's order (`screenshot.js:1305-1342`).
    pub const ROW: [CaptureType; 3] = [
        CaptureType::Selection,
        CaptureType::Screen,
        CaptureType::Window,
    ];

    /// The button's glyph.
    pub fn icon(self) -> &'static str {
        match self {
            CaptureType::Selection => "screenshot-ui-area-symbolic",
            CaptureType::Screen => "screenshot-ui-display-symbolic",
            CaptureType::Window => "screenshot-ui-window-symbolic",
        }
    }

    /// The button's caption.
    pub fn label(self) -> &'static str {
        match self {
            CaptureType::Selection => "Selection",
            CaptureType::Screen => "Screen",
            CaptureType::Window => "Window",
        }
    }
}

/// A clickable control in the panel. The panel is one baked texture, so hit-testing is geometry
/// against [`PanelLayout`] rather than a tree walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Type(CaptureType),
    /// The shot/cast segment at this index. Only index 0 (shot) exists until slice 4.
    ShotCast(usize),
    Capture,
    ShowPointer,
    /// **Our divergence**: arm the capture to fire after a delay. See
    /// `docs/fork/screenshot-ui-port.md`.
    Delay,
    Close,
}

impl Control {
    /// The control's tooltip, or `None` for one GNOME gives no tip.
    ///
    /// Deliberately not the button's own caption: the type buttons read "Selection" and say
    /// "Area Selection" (`js/ui/screenshot.js:1314-1348`), which is the point of having both.
    pub fn tooltip(self) -> Option<&'static str> {
        Some(match self {
            Control::Type(CaptureType::Selection) => "Area Selection",
            Control::Type(CaptureType::Screen) => "Screen Selection",
            Control::Type(CaptureType::Window) => "Window Selection",
            Control::ShotCast(0) => "Take Screenshot",
            Control::ShotCast(1) => "Record Screen",
            // `Record Screen`, once slice 4 puts a cast segment there.
            Control::ShotCast(_) => return None,
            Control::Capture => "Capture",
            Control::ShowPointer => "Show Pointer",
            Control::Delay => "Delay",
            // GNOME's close button carries no tooltip.
            Control::Close => return None,
        })
    }
}

/// Where every control sits inside the panel, in panel-local **logical** coordinates.
///
/// One layout feeds the bake, the icon placement and the hit test, so a control cannot be drawn
/// somewhere the pointer does not find it. Built from the caption sizes, which need a shaper —
/// hence it is produced during the bake and cached, and the panel is not clickable until one frame
/// has drawn (the same caveat the capture button already carried).
#[derive(Debug, Clone, Copy)]
pub struct PanelLayout {
    pub size: Size<f64, Logical>,
    /// Indexed by [`CaptureType::ROW`].
    pub type_buttons: [Rectangle<f64, Logical>; 3],
    pub shot_cast: Rectangle<f64, Logical>,
    pub capture: Rectangle<f64, Logical>,
    pub show_pointer: Rectangle<f64, Logical>,
    /// Our delay button, left of the show-pointer toggle. Both are persistent capture *options*,
    /// which is why they share the end of the bottom row rather than joining the type row.
    pub delay: Rectangle<f64, Logical>,
}

impl PanelLayout {
    /// `padding: $screenshot_ui_panel_padding` = `$base_padding * 3` (`_screenshot.scss:3,10`).
    const PAD: f64 = 18.;
    /// `padding-bottom: … - $base_padding` — trimmed "to accommodate the large capture button"
    /// (`_screenshot.scss:11-12`).
    const PAD_BOTTOM: f64 = 12.;
    /// `spacing: $base_padding * 2` between the type row and the bottom row
    /// (`_screenshot.scss:14`).
    const ROW_SPACING: f64 = 12.;
    /// `.screenshot-ui-type-button-container` spacing (`screenshot.js:1299`).
    const TYPE_SPACING: f64 = 12.;
    /// `.screenshot-ui-capture-button`: a `$large_icon_size` box inside `$base_margin` padding and
    /// a 4px border (`_screenshot.scss:40-45`).
    pub const CAPTURE_DIAMETER: f64 = 32. + 4. * 2. + 4. * 2.;
    /// `.screenshot-ui-show-pointer-button` extends `.icon-button`, whose padding is
    /// `$scaled_padding * 2` (`_buttons.scss:18-38`).
    const SHOW_POINTER_PAD: f64 = 12.;

    /// Lay the panel out around the measured captions.
    ///
    /// The bottom row is a **BinLayout** in GNOME (`screenshot.js:1350`): the shot/cast pill, the
    /// capture button and the show-pointer button all occupy the same band, aligned start, centre
    /// and end. Laying them out in sequence instead would push the capture button off-centre,
    /// which is the mistake this arithmetic exists to prevent.
    pub fn new(metrics: CaptionMetrics) -> Self {
        let CaptionMetrics { label_w, label_h } = metrics;
        let type_sizes = label_w.map(|w| widget::IconLabelButton::size(w, label_h));

        // `homogeneous: true` (`screenshot.js:1300`) — every type button is as wide as the widest.
        let type_w = type_sizes.iter().fold(0f64, |acc, s| acc.max(s.w));
        let type_h = type_sizes.iter().fold(0f64, |acc, s| acc.max(s.h));
        let type_row_w = type_w * 3. + Self::TYPE_SPACING * 2.;

        let shot_cast_size = widget::Segmented::size(2);
        let show_pointer_d =
            widget::IconButton::diameter(widget::IconButton::ICON_PX, Self::SHOW_POINTER_PAD);
        let bottom_h = Self::CAPTURE_DIAMETER
            .max(shot_cast_size.h)
            .max(show_pointer_d);

        // The bottom row must fit its three children side by side even though they are stacked in
        // a bin — otherwise the pill and the toggle would overlap the capture button on a narrow
        // panel.
        let bottom_min_w = shot_cast_size.w
            + Self::CAPTURE_DIAMETER
            + show_pointer_d * 2.
            + Self::TYPE_SPACING
            + Self::ROW_SPACING * 2.;
        let content_w = type_row_w.max(bottom_min_w);

        let size = Size::from((
            content_w + Self::PAD * 2.,
            Self::PAD + type_h + Self::ROW_SPACING + bottom_h + Self::PAD_BOTTOM,
        ));

        let type_y = Self::PAD;
        let type_x0 = Self::PAD + (content_w - type_row_w) / 2.;
        let type_buttons = [0usize, 1, 2].map(|i| {
            Rectangle::new(
                Point::from((type_x0 + (type_w + Self::TYPE_SPACING) * i as f64, type_y)),
                Size::from((type_w, type_h)),
            )
        });

        let bottom_y = Self::PAD + type_h + Self::ROW_SPACING;
        // Centre each child on the band, then align it start / centre / end.
        let centred = |w: f64, h: f64, x: f64| {
            Rectangle::new(
                Point::from((x, bottom_y + (bottom_h - h) / 2.)),
                Size::from((w, h)),
            )
        };
        let shot_cast = centred(shot_cast_size.w, shot_cast_size.h, Self::PAD);
        let capture = centred(
            Self::CAPTURE_DIAMETER,
            Self::CAPTURE_DIAMETER,
            (size.w - Self::CAPTURE_DIAMETER) / 2.,
        );
        let show_pointer = centred(
            show_pointer_d,
            show_pointer_d,
            size.w - Self::PAD - show_pointer_d,
        );
        let delay = centred(
            show_pointer_d,
            show_pointer_d,
            show_pointer.loc.x - Self::TYPE_SPACING - show_pointer_d,
        );

        Self {
            size,
            type_buttons,
            shot_cast,
            capture,
            show_pointer,
            delay,
        }
    }

    /// The control at `p`, in panel-local logical coordinates.
    ///
    /// The capture button and the show-pointer toggle are **round**, so they are hit-tested as
    /// circles: a click in the corner of a circular button's box reads as a misfire, not as a
    /// generous target (the same rule [`widget::IconButton::contains`] follows).
    pub fn control_at(&self, p: Point<f64, Logical>) -> Option<Control> {
        let in_circle = |r: Rectangle<f64, Logical>| {
            let radius = r.size.w.min(r.size.h) / 2.;
            let cx = r.loc.x + r.size.w / 2.;
            let cy = r.loc.y + r.size.h / 2.;
            let (dx, dy) = (p.x - cx, p.y - cy);
            dx * dx + dy * dy <= radius * radius
        };

        if in_circle(self.capture) {
            return Some(Control::Capture);
        }
        if in_circle(self.show_pointer) {
            return Some(Control::ShowPointer);
        }
        if in_circle(self.delay) {
            return Some(Control::Delay);
        }
        for (i, ty) in CaptureType::ROW.iter().enumerate() {
            if self.type_buttons[i].contains(p) {
                return Some(Control::Type(*ty));
            }
        }
        if self.shot_cast.contains(p) {
            for i in 0..2 {
                let seg = widget::Segmented::segment_rect(self.shot_cast, i);
                if seg.contains(p) {
                    return Some(Control::ShotCast(i));
                }
            }
        }
        None
    }
}

/// `.screenshot-ui-window-selector-window-container { margin: 100px }`, with `margin-bottom:
/// 200px` on the monitor carrying the panel — "make some room for the panel" (`_screenshot.scss:
/// 142-151`). We draw the panel on every output, so every output gets the taller bottom margin.
const SELECTOR_MARGIN: f64 = 100.;
const SELECTOR_MARGIN_BOTTOM: f64 = 200.;

/// The area the Window-mode selector may lay windows out in, given the output's logical size.
fn window_selector_area(output: Size<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((SELECTOR_MARGIN, SELECTOR_MARGIN)),
        Size::from((
            f64::max(output.w - SELECTOR_MARGIN * 2., 1.),
            f64::max(output.h - SELECTOR_MARGIN - SELECTOR_MARGIN_BOTTOM, 1.),
        )),
    )
}

/// `.screenshot-ui-window-selector { background-color: $system_base_color }`
/// (`_screenshot.scss:140`) — `$system_base_color` is `#26262a` (`_colors.scss:46`). Window mode
/// hides the frozen screen behind an opaque backdrop rather than shading it, so a window that is
/// not offered cannot be read off the wallpaper behind the selector.
const SELECTOR_BG: [f32; 4] = [0.149, 0.149, 0.165, 1.];

/// `.screenshot-ui-window-selector-window-border { border: 6px transparent; border-radius:
/// $modal_radius }` (`_screenshot.scss:154-158`), tinted on hover/checked by
/// `_screenshot.scss:168-184`.
const SELECTOR_BORDER: f64 = 6.;
const SELECTOR_RADIUS: f64 = 16.;

/// A window-selector border's state (`_screenshot.scss:168-184`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectorBorder {
    /// `:hover` — `st-darken(-st-accent-color, 15%)`, border only.
    Hovered,
    /// `:checked` — the accent border plus an accent wash at 20%.
    Checked,
}

impl SelectorBorder {
    /// `(border, fill)` for this state.
    fn colors(self, accent: Rgba) -> (Rgba, Option<Rgba>) {
        match self {
            Self::Hovered => (
                [accent[0] * 0.85, accent[1] * 0.85, accent[2] * 0.85, 1.],
                None,
            ),
            Self::Checked => (
                accent,
                // `background-color: st-transparentize(-st-accent-color, 0.8)`.
                Some([accent[0], accent[1], accent[2], 0.2]),
            ),
        }
    }
}

/// The centre of a rect.
fn centre(r: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
}

fn panel_location(output_data: &OutputData, panel_size: Size<i32, Buffer>) -> Point<i32, Physical> {
    let scale = output_data.scale;
    let margin: i32 = to_physical_precise_round(scale, panel_margin_bottom());
    let x = max(0, (output_data.size.w - panel_size.w) / 2);
    let y = max(0, output_data.size.h - panel_size.h - margin);
    Point::from((x, y))
}

/// The close button's rect for a panel drawn at `panel`, in the same space.
///
/// GNOME binds the button's *centre* to a panel corner with the same `AlignConstraint` pair the
/// overview's preview close button uses (`screenshot.js:1252-1266`), then insets it by
/// `margin-top`/`margin-right`. Clutter applies a margin by shrinking the allocation the constraint
/// positioned, which moves the painted box by half the margin — hence `CLOSE_MARGIN / 2.` rather
/// than the whole of it.
///
/// DIVERGENCE: `Meta.prefs_get_button_layout()` can put the close button on the left, and GNOME
/// flips the constraint's factor to match (`_refreshButtonLayout`, `screenshot.js:1533-1546`). We
/// always draw it on the right — the same divergence, for the same reason, as
/// `window_preview::close_rect`.
fn close_rect(panel: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let centre = Point::from((
        panel.loc.x + panel.size.w - CLOSE_MARGIN / 2.,
        panel.loc.y + CLOSE_MARGIN / 2.,
    ));
    Rectangle::new(
        centre - Point::from((CLOSE_SIZE / 2., CLOSE_SIZE / 2.)),
        Size::from((CLOSE_SIZE, CLOSE_SIZE)),
    )
}

/// Bake the close button's disc — `.window-close`'s fill, which `.screenshot-ui-close-button`
/// inherits wholesale (`_screenshot.scss:18`), so the colours come from `window_preview` rather
/// than a second copy. Its glyph is composited on top by [`OutputData::push_icons`].
///
/// Its `box-shadow` (`_screenshot.scss:21`) is not drawn: the shadow would need bleed room outside
/// this tightly-sized buffer, and the overview's identical button has never drawn one either.
fn generate_close_button(
    renderer: &mut VulkanRenderer,
    scale: f64,
    hovered: bool,
) -> anyhow::Result<VkTexture> {
    let size = widget::physical_size(scale, Size::from((CLOSE_SIZE, CLOSE_SIZE)));
    widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear(style::TRANSPARENT)?;
        let bg = if hovered { CLOSE_BG_HOVER } else { CLOSE_BG };
        // `border-radius: $forced_circular_radius` — a full circle.
        p.fill_rounded_full(CLOSE_SIZE / 2., bg)?;
        Ok(())
    })
}

/// Draw the screenshot help panel straight into a `VkTexture`: a dark box with a grey border, the
/// concentric-circle capture button on the left, and two left-aligned help lines with `Space`/`P`
/// keycaps on grey patches. `verb` is the pointer line's verb ("show" or "hide"). No cairo/pango.
/// Physical `(margin, offset_y)` of the panel drop shadow at `scale`: the blur bleed (~3σ) and the
/// downward offset. The shadow buffer pads the panel footprint by `margin` on top/left/right and
/// `margin + offset_y` at the bottom, and the element is placed at `panel_location − (margin,
/// margin)`.
fn shadow_pad(scale: f64) -> (i32, i32) {
    let sigma = SHADOW_BLUR * scale / 2.;
    let margin = (sigma * 3.).ceil() as i32;
    let offset_y = (SHADOW_OFFSET_Y * scale).round() as i32;
    (margin, offset_y)
}

/// Bake the panel's drop shadow into its own transparent `VkTexture` via [`Painter::drop_shadow`],
/// sized to hold the blur bleed + offset around a `panel_size` card. Composited behind the panel.
fn generate_panel_shadow(
    renderer: &mut VulkanRenderer,
    scale: f64,
    panel_size: Size<i32, Physical>,
) -> anyhow::Result<VkTexture> {
    let (margin, offset_y) = shadow_pad(scale);
    let size = Size::<i32, Physical>::from((
        panel_size.w + margin * 2,
        panel_size.h + margin * 2 + offset_y,
    ));
    widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear([0., 0., 0., 0.])?;
        // The panel footprint sits at (margin, margin) in the buffer; `drop_shadow` shifts it down
        // by the offset and blurs it. Logical coords (the verb re-multiplies by scale).
        let box_logical = Rectangle::new(
            Point::from((f64::from(margin) / scale, f64::from(margin) / scale)),
            Size::from((
                f64::from(panel_size.w) / scale,
                f64::from(panel_size.h) / scale,
            )),
        );
        p.drop_shadow(
            box_logical,
            PANEL_RADIUS,
            SHADOW_BLUR,
            (0., SHADOW_OFFSET_Y),
            SHADOW_COLOR,
        )?;
        Ok(())
    })
}

/// The handle texture's physical size, and the shadow margin inside it. The circle sits at
/// `(margin, margin)`; the buffer is larger so the shadow has room to spread and blur.
fn handle_pad(scale: f64) -> (i32, i32) {
    let sigma = HANDLE_SHADOW_BLUR * scale / 2.;
    let spread = HANDLE_SHADOW_SPREAD * scale;
    let offset = HANDLE_SHADOW_OFFSET_Y * scale;
    let margin = (sigma * 3. + spread + offset).ceil() as i32;
    let side = (HANDLE_PX * scale).round() as i32;
    (side, margin)
}

/// Bake one `.screenshot-ui-area-selector-handle`: a white circle over its own drop shadow.
fn generate_handle(renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
    let (side, margin) = handle_pad(scale);
    let size = Size::<i32, Physical>::from((side + margin * 2, side + margin * 2));

    widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear(style::TRANSPARENT)?;

        let logical = f64::from(margin) / scale;
        let circle = Rectangle::new(
            Point::from((logical, logical)),
            Size::from((HANDLE_PX, HANDLE_PX)),
        );
        // `box-shadow: 0 1px 3px 2px` — the 2px spread is the box grown before blurring, which is
        // what a spread *is*; our `drop_shadow` verb has blur and offset but no spread term.
        let spread = Rectangle::new(
            circle.loc - Point::from((HANDLE_SHADOW_SPREAD, HANDLE_SHADOW_SPREAD)),
            circle.size + Size::from((HANDLE_SHADOW_SPREAD * 2., HANDLE_SHADOW_SPREAD * 2.)),
        );
        p.drop_shadow(
            spread,
            spread.size.h / 2.,
            HANDLE_SHADOW_BLUR,
            (0., HANDLE_SHADOW_OFFSET_Y),
            SHADOW_COLOR,
        )?;
        p.fill_rounded(circle, HANDLE_PX / 2., [1., 1., 1., 1.])?;
        Ok(())
    })
}

/// Bake GNOME's control panel: the `%osd_panel` card, the three type buttons with their captions,
/// the shot/cast pill, the show-pointer toggle and the capture button.
///
/// Returns the [`PanelLayout`] it drew against, because that same layout is what places the glyphs
/// and answers the hit test — the panel is one texture, so nothing else knows where its controls
/// are. The captions are measured here (the panel is content-sized), which is why the layout cannot
/// be computed before a renderer exists.
fn generate_panel(
    renderer: &mut VulkanRenderer,
    scale: f64,
    accent: Rgba,
    state: PanelState,
) -> anyhow::Result<(VkTexture, PanelLayout)> {
    let _span = tracy_client::span!("screenshot_ui::generate_panel");

    let captions: Vec<ShapedText> = {
        let mut shaper = TextShaper::new(renderer, scale);
        CaptureType::ROW
            .iter()
            .map(|ty| {
                shaper.shape(
                    ty.label(),
                    TextStyle::new(widget::IconLabelButton::LABEL_PT),
                )
            })
            .collect::<anyhow::Result<_>>()?
    };

    // Ink is physical; the layout is logical.
    let mut label_w = [0f64; 3];
    for (i, run) in captions.iter().enumerate() {
        label_w[i] = f64::from(run.ink_bounds().2) / scale;
    }
    let label_h = captions
        .iter()
        .map(|r| r.line_box_height())
        .max()
        .unwrap_or(0);
    let metrics = CaptionMetrics {
        label_w,
        label_h: f64::from(label_h) / scale,
    };

    // The armed delay renders as its own number inside the button; off renders as an alarm glyph.
    let delay_label = (state.delay != 0)
        .then(|| {
            let mut shaper = TextShaper::new(renderer, scale);
            shaper.shape(
                &state.delay.to_string(),
                TextStyle::new(widget::IconLabelButton::LABEL_PT),
            )
        })
        .transpose()?;

    let layout = PanelLayout::new(metrics);
    let size = widget::physical_size(scale, layout.size);

    let texture = widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear(style::TRANSPARENT)?;

        // A rounded `%osd_panel` card: the fill, then its 1px white@2% inset border. The drop
        // shadow is a separate element composited under it (see `OutputData::shadow_element`), so
        // this buffer stays exactly panel-sized.
        p.fill_rounded_full(PANEL_RADIUS, style::OSD_BG)?;
        p.stroke_rounded_full(PANEL_RADIUS, 1., PANEL_BORDER_COLOR)?;

        for (i, ty) in CaptureType::ROW.iter().enumerate() {
            let control = Control::Type(*ty);
            // Hover is filtered at its source (`update_hover`), so an insensitive button can never
            // arrive here hovered — only its foreground differs.
            let button = widget::IconLabelButton::new(layout.type_buttons[i])
                .checked(state.capture_type == *ty)
                .hovered(state.hover == Some(control))
                .active(state.active == Some(control));
            p.icon_label_button(&button, accent)?;
            p.text(
                &captions[i],
                button.label_centre(metrics.label_h),
                Align::CENTER,
                type_fg(state.enables(*ty)),
            )?;
        }

        p.segmented(
            layout.shot_cast,
            2,
            state.mode.index(),
            match state.hover {
                Some(Control::ShotCast(i)) => Some(i),
                _ => None,
            },
        )?;

        // `%osd_button_flat` + `.icon-button`: a circle whose *fill* already encodes hover and
        // checked (see `style::OSD_FLAT_HOVER`), so `IconButton`'s generic hover wash would double
        // it — the flat cascade is resolved here instead.
        let pointer_control = Control::ShowPointer;
        let pointer_bg = if state.active == Some(pointer_control) {
            style::OSD_FLAT_ACTIVE
        } else if state.show_pointer {
            style::OSD_FLAT_CHECKED
        } else if state.hover == Some(pointer_control) {
            style::OSD_FLAT_HOVER
        } else {
            style::OSD_BG
        };
        p.icon_button(
            &widget::IconButton::new(layout.show_pointer, widget::IconButton::ICON_PX, pointer_bg),
            accent,
        )?;

        // The delay button shares the show-pointer cascade: an armed delay reads as `:checked`.
        let delay_control = Control::Delay;
        let delay_bg = if state.active == Some(delay_control) {
            style::OSD_FLAT_ACTIVE
        } else if state.delay != 0 {
            style::OSD_FLAT_CHECKED
        } else if state.hover == Some(delay_control) {
            style::OSD_FLAT_HOVER
        } else {
            style::OSD_BG
        };
        p.icon_button(
            &widget::IconButton::new(layout.delay, widget::IconButton::ICON_PX, delay_bg),
            accent,
        )?;
        if let Some(label) = &delay_label {
            p.text(label, centre(layout.delay), Align::CENTER, style::OSD_FG)?;
        }

        // The capture button: a real 4px ring, a transparent gap, then the inner circle.
        let cap = layout.capture;
        p.stroke_rounded(cap, cap.size.w / 2., CAPTURE_BORDER, style::OSD_FG)?;
        let inset = CAPTURE_BORDER + CAPTURE_GAP;
        let inner = Rectangle::new(
            cap.loc + Point::from((inset, inset)),
            Size::from((CAPTURE_INNER, CAPTURE_INNER)),
        );
        let cast = state.mode == CaptureMode::Cast;
        let circle = if state.active == Some(Control::Capture) {
            if cast {
                CAST_CIRCLE_ACTIVE
            } else {
                CAPTURE_CIRCLE_ACTIVE
            }
        } else if state.hover == Some(Control::Capture) {
            if cast {
                CAST_CIRCLE_HOVER
            } else {
                CAPTURE_CIRCLE_HOVER
            }
        } else if cast {
            CAST_CIRCLE
        } else {
            CAPTURE_CIRCLE
        };
        p.fill_rounded(inner, CAPTURE_INNER / 2., circle)?;

        Ok(())
    })?;

    Ok((texture, layout))
}

// === Delayed-capture countdown =================================================================

/// `.screenshot-ui-area-indicator-shade`, `_screenshot.scss:117-119`. Note this is the *base* rule,
/// not the 50% one: that one is nested under `.screenshot-ui-area-selector`, so it applies while
/// the picker is up and not to the standalone indicator below.
const CAST_SHADE: [f32; 4] = [0., 0., 0., 0.3];

/// The shade GNOME leaves over everything *outside* an area recording for as long as it runs
/// (`_screencastAreaIndicator`, `js/ui/screenshot.js:1192-1207`, geometry set in `_startScreencast`
/// at `:2022-2032`). It is the only sign on screen of *what* is being recorded, so it outlives the
/// picker that started it.
///
/// **Drawn only on [`RenderTarget::Output`].** GNOME can afford to put its copy straight on the
/// stage because the shade is by construction outside the recorded rect and so never in frame; we
/// fail closed instead and keep it off every capture target, which also spares a screenshot taken
/// mid-recording from inheriting it.
#[derive(Default)]
pub struct CastAreaIndicator {
    /// The output and its recorded rect in that output's physical pixels, while one is running.
    area: Option<(Output, Rectangle<i32, Physical>)>,
    shades: RefCell<[SolidColorBuffer; 4]>,
}

impl CastAreaIndicator {
    /// Start marking `rect` on `output`. A full-output rect collapses every shade to nothing, which
    /// is exactly what GNOME's Screen mode draws.
    pub fn set(&mut self, output: Output, rect: Rectangle<i32, Physical>) {
        *self.shades.borrow_mut() =
            std::array::from_fn(|_| SolidColorBuffer::new((0., 0.), CAST_SHADE));
        self.area = Some((output, rect));
    }

    pub fn clear(&mut self) {
        self.area = None;
    }

    /// Drop the mark only if it belongs to `output` — another output's recording is still running.
    pub fn clear_for_output(&mut self, output: &Output) {
        if self.area.as_ref().is_some_and(|(o, _)| o == output) {
            self.area = None;
        }
    }

    #[cfg(test)]
    pub fn area(&self) -> Option<(&Output, Rectangle<i32, Physical>)> {
        self.area.as_ref().map(|(o, r)| (o, *r))
    }

    /// Push the four shade rects for `output`, top / bottom / left / right, as in `UIAreaIndicator`
    /// (`js/ui/screenshot.js:145-245`): the horizontal pair spans the full width, the vertical pair
    /// fills only the band beside the selection.
    pub fn push(
        &self,
        target: RenderTarget,
        output: &Output,
        mut push: impl FnMut(SolidColorRenderElement),
    ) {
        if target != RenderTarget::Output {
            return;
        }
        let Some((recorded, rect)) = &self.area else {
            return;
        };
        if recorded != output {
            return;
        }

        let scale = output.current_scale().fractional_scale();
        let mode = output.current_mode().expect("a recorded output has a mode");
        let size = output.current_transform().transform_size(mode.size);

        // Clamp so a stale rect — an output that shrank under a running recording — can only shrink
        // the shades, never hand a negative size to a buffer.
        let rect = rect
            .intersection(Rectangle::from_size(size))
            .unwrap_or_default();
        let geo: [(Point<i32, Physical>, Size<i32, Physical>); 4] = [
            (Point::from((0, 0)), Size::from((size.w, rect.loc.y))),
            (
                Point::from((0, rect.loc.y + rect.size.h)),
                Size::from((size.w, size.h - rect.loc.y - rect.size.h)),
            ),
            (
                Point::from((0, rect.loc.y)),
                Size::from((rect.loc.x, rect.size.h)),
            ),
            (
                Point::from((rect.loc.x + rect.size.w, rect.loc.y)),
                Size::from((size.w - rect.loc.x - rect.size.w, rect.size.h)),
            ),
        ];

        let mut shades = self.shades.borrow_mut();
        for (buffer, (loc, size)) in zip(shades.iter_mut(), geo) {
            let size = Size::<i32, Physical>::from((size.w.max(0), size.h.max(0)));
            buffer.resize(size.to_f64().to_logical(scale));
            push(SolidColorRenderElement::from_buffer(
                buffer,
                loc.to_f64().to_logical(scale),
                1.,
                Kind::Unspecified,
            ));
        }
    }
}

/// The card's side and corner radius, and the point size of the number inside it.
const COUNTDOWN_SIDE: f64 = 108.;
const COUNTDOWN_RADIUS: f64 = 24.;
const COUNTDOWN_PT: f64 = 48.;

/// The on-screen countdown for a delayed capture.
///
/// **Drawn only on [`RenderTarget::Output`]** — see [`Countdown::element`]. A delay exists so the
/// shot can be taken with the shell out of the way; a countdown that could reach a screenshot, a
/// screencast or a portal capture would defeat exactly that.
#[derive(Default)]
pub struct Countdown {
    cache: RefCell<CountdownCache>,
}

#[derive(Default)]
struct CountdownCache {
    seconds: u64,
    scale: f64,
    context: Option<ContextId<VkTexture>>,
    texture: Option<VkTexture>,
}

impl Countdown {
    /// The countdown card for `output`, centred, or `None` when there is nothing to count down,
    /// the target is not the screen, or the bake failed.
    pub fn element(
        &self,
        renderer: &mut VulkanRenderer,
        target: RenderTarget,
        output: &Output,
        seconds: u64,
    ) -> Option<CapturedTextureRenderElement> {
        // The fail-closed rule this whole overlay exists under.
        if target != RenderTarget::Output || seconds == 0 {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let context = renderer.context_id();
        {
            let cache = self.cache.borrow();
            if cache.texture.is_none()
                || cache.seconds != seconds
                || cache.scale != scale
                || cache.context.as_ref() != Some(&context)
            {
                drop(cache);
                let texture = generate_countdown(renderer, scale, seconds)
                    .map_err(|err| warn!("error rendering the capture countdown: {err:?}"))
                    .ok();
                *self.cache.borrow_mut() = CountdownCache {
                    seconds,
                    scale,
                    context: Some(context),
                    texture,
                };
            }
        }

        let texture = self.cache.borrow().texture.clone()?;
        let size = crate::utils::output_size(output);
        let location = Point::from((
            (size.w - COUNTDOWN_SIDE) / 2.,
            (size.h - COUNTDOWN_SIDE) / 2.,
        ));

        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                buffer,
                location,
                1.,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }
}

fn generate_countdown(
    renderer: &mut VulkanRenderer,
    scale: f64,
    seconds: u64,
) -> anyhow::Result<VkTexture> {
    let label = {
        let mut shaper = TextShaper::new(renderer, scale);
        shaper.shape(&seconds.to_string(), TextStyle::new(COUNTDOWN_PT))?
    };

    let logical = Size::from((COUNTDOWN_SIDE, COUNTDOWN_SIDE));
    let size = widget::physical_size(scale, logical);
    widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);
        p.clear(style::TRANSPARENT)?;
        p.fill_rounded_full(COUNTDOWN_RADIUS, style::OSD_BG)?;
        p.stroke_rounded_full(COUNTDOWN_RADIUS, 1., PANEL_BORDER_COLOR)?;
        p.text(
            &label,
            Point::from((COUNTDOWN_SIDE / 2., COUNTDOWN_SIDE / 2.)),
            Align::CENTER,
            style::OSD_FG,
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LABEL_W: [f64; 3] = CaptionMetrics::TEST.label_w;
    const LABEL_H: f64 = CaptionMetrics::TEST.label_h;

    fn layout() -> PanelLayout {
        PanelLayout::new(CaptionMetrics::TEST)
    }

    // GNOME's bottom row is a BinLayout (`screenshot.js:1350`), so the capture button is centred on
    // the *panel*, with the pill and the toggle aligned to the ends of the same band. Laying the
    // three out in sequence — the reflex, and what a BoxLayout would give — puts the capture button
    // wherever the pill's width leaves it, which is visibly off-centre and the whole reason this
    // arithmetic exists.
    #[test]
    fn the_capture_button_is_centred_on_the_panel_not_after_the_pill() {
        let l = layout();

        let capture_centre = l.capture.loc.x + l.capture.size.w / 2.;
        assert!(
            (capture_centre - l.size.w / 2.).abs() < 0.001,
            "capture button at {capture_centre}, panel centre {}",
            l.size.w / 2.
        );

        // The pill starts at the left padding and the toggle ends at the right padding.
        assert_eq!(l.shot_cast.loc.x, PanelLayout::PAD);
        assert_eq!(
            l.show_pointer.loc.x + l.show_pointer.size.w,
            l.size.w - PanelLayout::PAD
        );

        // All three share the band, so their vertical centres agree.
        let mid = |r: Rectangle<f64, Logical>| r.loc.y + r.size.h / 2.;
        assert!((mid(l.capture) - mid(l.shot_cast)).abs() < 0.001);
        assert!((mid(l.capture) - mid(l.show_pointer)).abs() < 0.001);

        // ...and they do not overlap, which a bin layout alone would not guarantee.
        assert!(l.shot_cast.loc.x + l.shot_cast.size.w <= l.capture.loc.x);
        assert!(l.capture.loc.x + l.capture.size.w <= l.show_pointer.loc.x);
    }

    // `homogeneous: true` (`screenshot.js:1300`): every type button takes the width of the widest,
    // so the row does not jitter as the captions change with translation.
    fn mid_y(r: Rectangle<f64, Logical>) -> f64 {
        r.loc.y + r.size.h / 2.
    }

    #[test]
    fn the_type_buttons_are_homogeneous_and_evenly_spaced() {
        let l = layout();

        let w = l.type_buttons[0].size.w;
        for b in &l.type_buttons {
            assert_eq!(b.size.w, w);
            assert_eq!(b.size.h, l.type_buttons[0].size.h);
            assert_eq!(b.loc.y, PanelLayout::PAD);
        }
        for pair in l.type_buttons.windows(2) {
            assert_eq!(pair[1].loc.x - pair[0].loc.x, w + PanelLayout::TYPE_SPACING);
        }

        // The widest caption sets the width, not the first one.
        let widest = LABEL_W.iter().cloned().fold(0f64, f64::max);
        assert_eq!(w, widget::IconLabelButton::size(widest, LABEL_H).w);

        // The row is centred on the panel.
        let left = l.type_buttons[0].loc.x;
        let right = l.size.w - (l.type_buttons[2].loc.x + l.type_buttons[2].size.w);
        assert!(
            (left - right).abs() < 0.001,
            "type row is off-centre: {left} left, {right} right"
        );
    }

    // A click in the corner of a circular button's bounding box is an inch from any drawn pixel.
    // Treating it as a hit is the kind of generosity that fires the shutter when the user meant to
    // drag a selection.
    #[test]
    fn the_round_controls_are_hit_tested_as_circles() {
        let l = layout();

        assert_eq!(l.control_at(centre(l.capture)), Some(Control::Capture));
        assert_eq!(
            l.control_at(centre(l.show_pointer)),
            Some(Control::ShowPointer)
        );

        // The box corner of the capture button is outside the circle.
        let corner = Point::from((l.capture.loc.x + 1., l.capture.loc.y + 1.));
        assert!(l.capture.contains(corner));
        assert_ne!(l.control_at(corner), Some(Control::Capture));
    }

    // GNOME's `_computeCursorType` in one table. The rect is 100x100 at (100, 100) so every band
    // is reachable, and scale 1 keeps the thresholds at their logical values (handle radius 12,
    // edge band 10).
    #[test]
    fn the_selection_grabs_edges_corners_and_its_middle() {
        use Side::{High, Low};

        let rect = Rectangle::<i32, Physical>::new(Point::from((100, 100)), Size::from((100, 100)));
        let at = |x, y| area_target(rect, Point::from((x, y)), 1.);
        let resize = |x, y| AreaTarget::Resize { x, y };

        // Corners, on the handle.
        assert_eq!(at(100, 100), resize(Some(Low), Some(Low)));
        assert_eq!(at(199, 100), resize(Some(High), Some(Low)));
        assert_eq!(at(100, 199), resize(Some(Low), Some(High)));
        assert_eq!(at(199, 199), resize(Some(High), Some(High)));

        // The handle is a circle, so its bounding-box corner is not a grab: (11, 11) out is
        // outside a radius-12 disc even though each axis alone is within 12. It is past the 10px
        // edge bands too, so nothing is grabbed there at all.
        assert_eq!(at(89, 89), AreaTarget::Outside);
        // ...while straight up from the same corner, still on the disc, is.
        assert_eq!(at(100, 89), resize(Some(Low), Some(Low)));

        // Edges, from just outside — that is where the bands live.
        assert_eq!(at(95, 150), resize(Some(Low), None), "left edge");
        assert_eq!(at(204, 150), resize(Some(High), None), "right edge");
        assert_eq!(at(150, 95), resize(None, Some(Low)), "top edge");
        assert_eq!(at(150, 204), resize(None, Some(High)), "bottom edge");

        // The middle moves.
        assert_eq!(at(150, 150), AreaTarget::Move);

        // Past the bands, nothing is grabbed and a press starts a new rectangle.
        assert_eq!(at(150, 89), AreaTarget::Outside, "11px above the top edge");
        assert_eq!(
            at(89, 150),
            AreaTarget::Outside,
            "11px left of the left edge"
        );
        assert_eq!(at(50, 50), AreaTarget::Outside);
        assert_eq!(
            at(89, 250),
            AreaTarget::Outside,
            "in the left band on x but nowhere near on y"
        );
    }

    // The scale is not decoration: the bands and handles are logical px, so at scale 2 a point
    // that was outside is inside. A hit test that forgot it would be wrong by half on HiDPI.
    #[test]
    fn the_grab_bands_scale() {
        let rect = Rectangle::<i32, Physical>::new(Point::from((100, 100)), Size::from((100, 100)));
        let p = Point::from((85, 150));
        assert_eq!(area_target(rect, p, 1.), AreaTarget::Outside);
        assert_eq!(
            area_target(rect, p, 2.),
            AreaTarget::Resize {
                x: Some(Side::Low),
                y: None
            },
            "at scale 2 the 10px band is 20 physical px, and 15 away is inside it"
        );
    }

    // Every grab must advertise itself, and the eight resize cursors must not collide — a table
    // this regular is exactly where a copy-paste slip hides.
    #[test]
    fn every_grab_has_its_own_cursor() {
        use Side::{High, Low};

        let mut seen = Vec::new();
        for x in [Some(Low), Some(High), None] {
            for y in [Some(Low), Some(High), None] {
                if x.is_none() && y.is_none() {
                    continue;
                }
                seen.push(AreaTarget::Resize { x, y }.cursor());
            }
        }
        let mut sorted = seen.clone();
        sorted.sort_by_key(|c| format!("{c:?}"));
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            seen.len(),
            "two grabs share a cursor: {seen:?}"
        );

        assert_eq!(AreaTarget::Outside.cursor(), CursorIcon::Crosshair);
        assert_eq!(AreaTarget::Move.cursor(), CursorIcon::Move);
    }

    // The delay button is ours, and it shares the bottom row's right end with show-pointer: both
    // are persistent capture *options*, as against the type row's "what to capture".
    #[test]
    fn the_delay_button_sits_beside_show_pointer_and_takes_clicks() {
        let l = layout();

        assert_eq!(l.control_at(centre(l.delay)), Some(Control::Delay));
        assert_eq!(
            l.delay.size, l.show_pointer.size,
            "the two round toggles must match"
        );
        assert!(
            (mid_y(l.delay) - mid_y(l.show_pointer)).abs() < 0.001,
            "they share a row, so they share a centreline"
        );
        assert!(
            l.delay.loc.x + l.delay.size.w <= l.show_pointer.loc.x,
            "delay goes to the left of show-pointer, and they must not overlap"
        );
        assert!(
            l.capture.loc.x + l.capture.size.w <= l.delay.loc.x,
            "the capture button must still clear both of them"
        );
    }

    // Every type button is hit-testable, Window included: GNOME keeps it visible and greys it
    // out when there is nothing to pick, rather than removing it. Whether the click *does*
    // anything is `ScreenshotUi::set_capture_type`'s call, not the layout's — see
    // `the_window_button_is_inert_without_windows` in the corpus.
    #[test]
    fn every_type_button_takes_clicks_at_its_own_centre() {
        let l = layout();

        for (i, ty) in CaptureType::ROW.iter().enumerate() {
            assert_eq!(
                l.control_at(centre(l.type_buttons[i])),
                Some(Control::Type(*ty))
            );
        }
    }
}
