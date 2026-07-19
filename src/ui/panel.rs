//! The GNOME top panel.
//!
//! A persistent bar drawn in-compositor at the top of each output while the
//! session is in GNOME (floating) windowing mode. It draws the bar chrome, a
//! left-hand **workspace indicator** (the graphical dots that replaced GNOME's
//! old "Activities" text button — click toggles the overview, scroll switches
//! workspace) and a centered clock; the panel also reserves a top strut so
//! windows never sit under it (see `layout::workspace::compute_working_area`).
//!
//! The bar is drawn entirely on the GPU through the owned Vulkan renderer: an
//! offscreen `VkTexture` is cleared to the bar background, the clock glyph run
//! is drawn with the [`render_glyphs`](VulkanFrame::render_glyphs) material, and
//! the result is composited as a `TextureRenderElement` — no cairo/pango raster.
//! The workspace dots are drawn straight into the bar offscreen with the
//! [`render_rounded_rect`](VulkanFrame::render_rounded_rect) material (the active
//! dot a wide pill, the others small circles) — no CPU rasterization.
//!
//! ## Extension-representable structure
//!
//! The panel's *logical* model — which named items live in which of the three
//! boxes, and each item's screen rectangle and state — is kept separate from the
//! per-frame render path. GNOME extensions address the panel through exactly this
//! surface (`Main.panel.statusArea[role]`, the left/center/right boxes), so we
//! model it the same way ([`PanelBox`] / [`PanelItem`], roles [`ROLE_ACTIVITIES`]
//! and [`ROLE_DATE_MENU`]) even though the extension host itself is deferred. The
//! goal is a stable role→box→rect map an extension host can bind to, *not* a
//! widget tree; rendering consumes the model but never the other way around.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::null_mut;
use std::rc::Rc;
use std::time::Duration;

use niri_config::Config;
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::gnome::{ClockFormat, QuickToggles};
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::system_status::{self, SystemStatus};
use crate::utils::{output_size, to_physical_precise_round};

/// Logical height of the panel. GNOME's is `2.2em` at an `11pt` base font,
/// i.e. ~32px at scale 1 (`gnome-shell-sass/widgets/_panel.scss`).
pub const PANEL_HEIGHT: f64 = 32.;

/// Panel font size in logical pixels-per-em. The clock draws at GNOME's `panel_button`
/// base of 11pt (`_drawing.scss`), bold. Scaled by the output scale to the physical em
/// the glyph rasterizer shapes at.
const FONT_PX: f64 = crate::ui::pt_to_px(11.);

/// Base workspace-dot diameter, logical px. GNOME: `$scalable_icon_size (16) * 0.5`
/// (`gnome-shell-sass/widgets/_panel.scss`), fully rounded (`$forced_circular_radius`).
const DOT_DIAMETER: f64 = 8.;

/// Gap between dots, logical px (`panel.js` `WorkspaceIndicators` box `spacing`).
const DOT_SPACING: f64 = 5.;

/// Horizontal padding from the button's hit-rect edge to its content (the dot row
/// or the status-icon cluster), logical px: the pill's edge inset (`BTN_MARGIN_X`)
/// plus the panel_button breathing room (`BTN_H_PADDING`), so the content sits
/// `BTN_H_PADDING` inside the lit pill.
const INDICATOR_H_PADDING: f64 = BTN_MARGIN_X + BTN_H_PADDING;

/// Inactive dots are drawn at 0.75× and half-opacity (`panel.js`
/// `INACTIVE_WORKSPACE_DOT_SCALE`, `WorkspaceDot._updateVisuals`).
const INACTIVE_DOT_SCALE: f64 = 0.75;
const INACTIVE_DOT_OPACITY: f64 = 0.5;

/// Horizontal padding from the dateMenu (clock) button's hit-rect edge to the clock
/// label, logical px — the pill inset plus the panel_button breathing room, so the
/// clock sits `BTN_H_PADDING` inside its lit pill, like the other buttons.
const H_PADDING: f64 = BTN_MARGIN_X + BTN_H_PADDING;

/// Bar background (opaque black — GNOME's dark panel), straight RGBA.
const BAR_BG: [f32; 4] = [0., 0., 0., 1.];

/// Panel-button container inset from its hit rect (`_drawing.scss` `panel_button`
/// mixin): `$base_margin` (4px) horizontally so an edge button isn't glued to the
/// screen edge, and the 3px transparent border vertically. What's left is the
/// fully-rounded (`$forced_circular_radius`) pill that lights up on hover/active.
const BTN_MARGIN_X: f64 = 4.;
const BTN_INSET_Y: f64 = 3.;

/// Horizontal breathing room between the lit pill and the button's content, logical
/// px — gnome-shell's panel_button `-natural-hpadding` (`$base_padding * 2` = 12px,
/// `_panel.scss`). Without it the pill hugs the dots/icons; the button's content
/// padding is this plus the pill's own `BTN_MARGIN_X` edge inset.
const BTN_H_PADDING: f64 = 12.;

/// `panel_button` fill *alpha* over the dark bar (white `$fg`) — the SDF fill blends
/// over the opaque background: idle 0, hover `transparentize($fg, .83)`,
/// active/`:checked` `transparentize($fg, .72)`, active+hover `transparentize($fg, .68)`.
/// The container color is white at the (animated) alpha of these.
const BTN_HOVER_A: f32 = 0.17;
const BTN_ACTIVE_A: f32 = 0.28;
const BTN_ACTIVE_HOVER_A: f32 = 0.32;

/// The three panel-button roles whose containers fade between states.
const BTN_ROLES: [&str; 3] = [ROLE_ACTIVITIES, ROLE_DATE_MENU, ROLE_QUICK_SETTINGS];

/// A button container's fill-alpha fade (gnome-shell `panel_button`'s 150ms
/// `transition-duration`). `target` is the alpha the fill is heading to; `anim`
/// interpolates the previous alpha → target and is cleared once it settles.
struct FillFade {
    target: f32,
    anim: Option<Animation>,
}

impl FillFade {
    /// The current fill alpha: the running animation's value, or the settled target.
    fn value(&self) -> f32 {
        match &self.anim {
            Some(a) if !a.is_done() => a.clamped_value() as f32,
            _ => self.target,
        }
    }

    fn is_animating(&self) -> bool {
        self.anim.as_ref().is_some_and(|a| !a.is_done())
    }
}

/// A panel button's rounded container: its hit rect inset by the `panel_button`
/// margin/border, so the pill floats off the screen edge and the fill is a stadium.
fn container_rect(rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((rect.loc.x + BTN_MARGIN_X, rect.loc.y + BTN_INSET_Y)),
        Size::from((
            (rect.size.w - 2. * BTN_MARGIN_X).max(0.),
            (rect.size.h - 2. * BTN_INSET_Y).max(0.),
        )),
    )
}

/// Text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];

/// Role of the left-hand workspace indicator (GNOME's `activities` panel role).
pub const ROLE_ACTIVITIES: &str = "activities";
/// Role of the centered clock (GNOME's `dateMenu` panel role).
pub const ROLE_DATE_MENU: &str = "dateMenu";
/// Role of the right-hand status area that opens quick settings (GNOME's
/// `quickSettings`).
pub const ROLE_QUICK_SETTINGS: &str = "quickSettings";

/// Right-box status-indicator icon size and inter-icon gap, logical px.
const QS_ICON: f64 = 16.;
const QS_ICON_GAP: f64 = 4.;

/// Fallback anchor icon shown only when the status cluster would otherwise be
/// empty (no `dbus` feature / no daemons), so the button is always clickable.
/// First that resolves wins.
const QS_ANCHOR_ICONS: &[&str] = &[
    "emblem-system-symbolic",
    "applications-system-symbolic",
    "open-menu-symbolic",
];
/// Real status icons the indicator surfaces when the matching toggle is on
/// (GNOME shows the DND icon in the panel; the others are our own touch).
const QS_DND_ICONS: &[&str] = &["notifications-disabled-symbolic"];
const QS_NIGHT_ICONS: &[&str] = &["night-light-symbolic"];

/// The candidate icon-name lists for the quick-settings indicator, left-to-right:
/// active toggle touches (DND / Night Light), then the live system cluster
/// (network, then battery in the corner, like GNOME). Each entry is a candidate
/// list; the first name that resolves in the theme is drawn. Falls back to the
/// anchor icon so the cluster is never empty.
fn qs_indicator_icons(toggles: QuickToggles, status: &SystemStatus) -> Vec<Vec<String>> {
    let owned = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let mut v: Vec<Vec<String>> = Vec::new();
    if toggles.do_not_disturb {
        v.push(owned(QS_DND_ICONS));
    }
    if toggles.night_light {
        v.push(owned(QS_NIGHT_ICONS));
    }
    if let Some(candidates) = system_status::network_icon(status.network) {
        v.push(owned(candidates));
    }
    if let Some(battery) = &status.battery {
        v.push(system_status::battery_icon(battery));
    }
    if v.is_empty() {
        v.push(owned(QS_ANCHOR_ICONS));
    }
    v
}

/// Logical width of the right-box quick-settings indicator (padding + icons +
/// gaps). Depends on how many status icons are currently shown.
fn qs_indicator_width(toggles: QuickToggles, status: &SystemStatus) -> f64 {
    let n = qs_indicator_icons(toggles, status).len() as f64;
    2. * INDICATOR_H_PADDING + n * QS_ICON + (n - 1.) * QS_ICON_GAP
}

/// One of the panel's three boxes, mirroring GNOME's `_leftBox`/`_centerBox`/`_rightBox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelBox {
    Left,
    Center,
    Right,
}

/// A named panel component and where it currently sits. The addressable surface a
/// future extension host binds to (see the module docs); rendering is separate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelItem {
    /// Stable GNOME role name, e.g. [`ROLE_ACTIVITIES`].
    pub role: &'static str,
    /// Which box the item lives in.
    pub r#box: PanelBox,
    /// The item's rectangle in output-local logical coords.
    pub rect: Rectangle<f64, Logical>,
}

/// Per-output workspace snapshot that drives the dot indicator (GNOME's
/// `WorkspacesAdjustment`: `count` = `upper`, `active` = `value`). The panel is a
/// single global object rendered per output, so the caller passes the snapshot
/// for the output being drawn/hit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceState {
    pub count: usize,
    pub active: usize,
}

impl WorkspaceState {
    /// Clamp `active` into range so an out-of-range index never drops the wide pill.
    fn active(self) -> usize {
        self.active.min(self.count.saturating_sub(1))
    }
}

/// The dot row's `widthMultiplier` (`panel.js` `WorkspaceIndicators._updateExpansion`):
/// the active pill is this many base-diameters wide.
fn width_multiplier(count: usize) -> f64 {
    if count <= 2 {
        3.625
    } else if count <= 5 {
        3.25
    } else {
        2.75
    }
}

/// Logical width of the whole indicator button (padding + dots + gaps). Independent
/// of *which* dot is active (exactly one is the wide pill), so hit rect, checked
/// highlight, and the drawn bitmap all agree.
fn indicator_logical_width(count: usize) -> f64 {
    if count == 0 {
        return 2. * INDICATOR_H_PADDING;
    }
    let mult = width_multiplier(count);
    let dots = (count as f64 - 1.) * DOT_DIAMETER + DOT_DIAMETER * mult;
    let gaps = (count as f64 - 1.) * DOT_SPACING;
    2. * INDICATOR_H_PADDING + dots + gaps
}

/// Cached bar textures, keyed so a content change misses. Tied to a renderer
/// context: dropped wholesale when the renderer changes.
struct BarCache {
    context: Option<ContextId<VkTexture>>,
    /// Bar chrome keyed by (scale, physical width, workspace count, active index):
    /// the count sets the checked-highlight width, and the count + active index
    /// place the workspace dots (drawn into the bar as rounded rects).
    textures: HashMap<(NotNan<f64>, i32, usize, usize), VkTexture>,
    /// Uploaded quick-settings indicator icons, keyed by (scale, resolved name).
    /// Always tinted white, so the name is the only content key.
    qs_icons: HashMap<(NotNan<f64>, String), TextureBuffer<VkTexture>>,
}

impl BarCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
            qs_icons: HashMap::new(),
        }
    }

    /// Drop everything cached (content or renderer changed).
    fn clear(&mut self) {
        self.textures.clear();
        self.qs_icons.clear();
    }

    /// Drop only the bar chrome, keeping the uploaded status icons. Used when just
    /// the button container state (hover/active) changes, which redraws the bar but
    /// leaves the composited icons untouched — hover moves must not re-upload icons.
    fn clear_bars(&mut self) {
        self.textures.clear();
    }
}

pub struct Panel {
    /// Current clock string, e.g. "14:30". Recomputed on each clock tick.
    clock_text: String,
    /// How the clock label is formatted (from `org.gnome.desktop.interface`).
    clock_format: ClockFormat,
    /// Whether the overview is open (drives the Activities button's active state).
    activities_checked: bool,
    /// Which panel button is currently pointer-hovered, if any — its container
    /// lights up dimly (gnome-shell `panel_button:hover`). Set from pointer motion.
    hovered: Option<&'static str>,
    /// Which panel button's popover menu is up, if any — its container lights up
    /// strongly (`panel_button:checked`). Synced from the popover each frame.
    open_menu: Option<&'static str>,
    /// The quick-settings toggle states, mirrored from gsettings — they decide
    /// which status icons the right-box indicator shows.
    toggles: QuickToggles,
    /// Live network + battery state (from the system-bus watcher), shown as the
    /// right-box status cluster.
    system_status: SystemStatus,

    /// Animation clock + config, for the button-container fill fades.
    clock: Clock,
    config: Rc<RefCell<Config>>,
    /// Per-role container fill-alpha fade (keyed by role), so hover/active
    /// transitions ease over 150ms instead of snapping.
    fills: HashMap<&'static str, FillFade>,

    /// Cached GPU chrome, cleared whenever the drawn content changes.
    cache: RefCell<BarCache>,
}

impl Panel {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        let clock_format = ClockFormat::default();
        let fills = BTN_ROLES
            .iter()
            .map(|&role| {
                (
                    role,
                    FillFade {
                        target: 0.,
                        anim: None,
                    },
                )
            })
            .collect();
        Self {
            clock_text: format_clock(unsafe { libc::time(null_mut()) }, clock_format),
            clock_format,
            activities_checked: false,
            hovered: None,
            open_menu: None,
            toggles: QuickToggles::default(),
            system_status: SystemStatus::default(),
            clock,
            config,
            fills,
            cache: RefCell::new(BarCache::new()),
        }
    }

    /// Adopt the quick-settings toggle states (from a gsettings change or a tile
    /// click). Returns whether they changed (so the caller can queue a redraw);
    /// the indicator's icon set may differ.
    pub fn set_quick_toggles(&mut self, toggles: QuickToggles) -> bool {
        if toggles == self.toggles {
            return false;
        }
        self.toggles = toggles;
        self.cache.borrow_mut().clear();
        true
    }

    /// Adopt the live network/battery state (from the system-bus watcher). Returns
    /// whether it changed, so the caller can queue a redraw; the indicator's icon
    /// set (and width) may differ.
    pub fn set_system_status(&mut self, status: SystemStatus) -> bool {
        if status == self.system_status {
            return false;
        }
        self.system_status = status;
        self.cache.borrow_mut().clear();
        true
    }

    /// Recompute the clock from the wall clock. Returns whether it changed (so
    /// the caller can queue a redraw). `now` is epoch seconds — injectable so
    /// tests are deterministic.
    pub fn update_clock_at(&mut self, now: libc::time_t) -> bool {
        let text = format_clock(now, self.clock_format);
        if text != self.clock_text {
            self.clock_text = text;
            self.cache.borrow_mut().clear();
            true
        } else {
            false
        }
    }

    /// Recompute the clock from the current wall-clock time.
    pub fn update_clock(&mut self) -> bool {
        self.update_clock_at(unsafe { libc::time(null_mut()) })
    }

    /// Adopt a clock label format (from a gsettings change). Reformats the label
    /// immediately; returns whether the displayed string changed.
    pub fn set_clock_format(&mut self, format: ClockFormat) -> bool {
        if format == self.clock_format {
            return false;
        }
        self.clock_format = format;
        self.update_clock()
    }

    /// How long until the clock label needs redrawing: every second when it shows
    /// seconds, otherwise on the next minute boundary. The wake source that ticks
    /// the clock uses this so an idle desktop wakes no more than it must.
    pub fn clock_tick_interval(&self) -> Duration {
        if self.clock_format.show_seconds {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(secs_until_next_minute())
        }
    }

    /// Reflect the overview open/closed state on the Activities button.
    pub fn set_overview_open(&mut self, open: bool) {
        if open != self.activities_checked {
            self.activities_checked = open;
            self.retarget_fills();
        }
    }

    /// Set which panel button the pointer is hovering (`None` when off any button).
    /// Returns whether it changed, so the caller can queue a redraw; only the bar
    /// chrome is invalidated (the status icons are unaffected by hover).
    pub fn set_hovered_role(&mut self, role: Option<&'static str>) -> bool {
        if role == self.hovered {
            return false;
        }
        self.hovered = role;
        self.retarget_fills();
        true
    }

    /// Set which panel button's popover is open (`None` when none is). Returns
    /// whether it changed, so the caller can queue a redraw.
    pub fn set_open_menu(&mut self, role: Option<&'static str>) -> bool {
        if role == self.open_menu {
            return false;
        }
        self.open_menu = role;
        self.retarget_fills();
        true
    }

    /// The container fill *alpha* a button should settle at for its current
    /// hover/active state (0 when idle). A button is "active" when its menu is up
    /// (or, for Activities, the overview is open).
    fn target_alpha(&self, role: &str) -> f32 {
        let active = if role == ROLE_ACTIVITIES {
            self.activities_checked
        } else {
            self.open_menu == Some(role)
        };
        let hover = self.hovered == Some(role);
        match (active, hover) {
            (true, true) => BTN_ACTIVE_HOVER_A,
            (true, false) => BTN_ACTIVE_A,
            (false, true) => BTN_HOVER_A,
            (false, false) => 0.,
        }
    }

    /// Recompute every button's target fill alpha after a state change and, for any
    /// that changed, start a fade from the current (animated) value to the new
    /// target — gnome-shell's `panel_button` 150ms transition. Invalidates the bar
    /// cache so the fade redraws.
    fn retarget_fills(&mut self) {
        let config = self.config.borrow().animations.panel_popover_open_close.0;
        let mut changed = false;
        for role in BTN_ROLES {
            let target = self.target_alpha(role);
            let fade = self.fills.get_mut(role).expect("every role has a fade");
            if (fade.target - target).abs() < f32::EPSILON {
                continue;
            }
            let from = fade.value();
            fade.target = target;
            fade.anim = Some(Animation::new(
                self.clock.clone(),
                f64::from(from),
                f64::from(target),
                0.,
                config,
            ));
            changed = true;
        }
        if changed {
            self.cache.borrow_mut().clear_bars();
        }
    }

    /// Settle finished container-fill fades (drop the completed animation so it
    /// rests at its target). Called from the compositor's animation tick.
    pub fn advance_animations(&mut self) {
        for fade in self.fills.values_mut() {
            if fade.anim.as_ref().is_some_and(|a| a.is_done()) {
                fade.anim = None;
            }
        }
    }

    /// Whether any button-container fade is still running (keeps the redraw loop
    /// ticking, and makes `render` draw the bar fresh rather than from cache).
    pub fn are_animations_ongoing(&self) -> bool {
        self.fills.values().any(FillFade::is_animating)
    }

    /// The rounded containers to paint behind the buttons this frame, each a
    /// (pill rect, fill color) — only for buttons with a non-zero (animated) fill.
    /// The same building block (`render_rounded_rect`) for all three, so they're
    /// consistent. `output_width` places the centered/right-anchored buttons.
    fn button_containers(
        &self,
        output_width: f64,
        ws: WorkspaceState,
    ) -> Vec<(Rectangle<f64, Logical>, [f32; 4])> {
        let mut v = Vec::new();
        for (role, rect) in [
            (ROLE_ACTIVITIES, self.activities_rect(ws)),
            (ROLE_DATE_MENU, self.date_menu_rect(output_width)),
            (ROLE_QUICK_SETTINGS, self.quick_settings_rect(output_width)),
        ] {
            let alpha = self.fills.get(role).map_or(0., FillFade::value);
            if alpha > 0.001 {
                v.push((container_rect(rect), [1., 1., 1., alpha]));
            }
        }
        v
    }

    /// The current clock string (for tests / introspection).
    pub fn clock_text(&self) -> &str {
        &self.clock_text
    }

    /// Whether the indicator button is highlighted (for tests / introspection).
    pub fn activities_checked(&self) -> bool {
        self.activities_checked
    }

    /// The workspace-indicator button rect (left-anchored, so the same on every
    /// output). Width grows with the workspace count.
    pub fn activities_rect(&self, ws: WorkspaceState) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((0., 0.)),
            Size::from((indicator_logical_width(ws.count), PANEL_HEIGHT)),
        )
    }

    /// The dateMenu (clock) button rect: the shaped label plus a padding on each
    /// side, centered on the output. `output_width` is the output's logical width.
    pub fn date_menu_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        let clock_w = niri_vk::text::measure_line_width_weighted(&self.clock_text, FONT_PX as f32, true);
        let w = clock_w + H_PADDING * 2.;
        Rectangle::new(
            Point::from(((output_width - w) / 2., 0.)),
            Size::from((w, PANEL_HEIGHT)),
        )
    }

    /// The quick-settings status indicator rect: the icon cluster plus a padding
    /// on each side, right-anchored on the output. Its width tracks how many
    /// status icons (toggles + live network/battery) are currently shown.
    pub fn quick_settings_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        let w = qs_indicator_width(self.toggles, &self.system_status);
        Rectangle::new(
            Point::from((output_width - w, 0.)),
            Size::from((w, PANEL_HEIGHT)),
        )
    }

    /// The panel's items with their current rectangles, for introspection and the
    /// (deferred) extension host. `output_width` is the output's logical width, used
    /// to place the centered clock and the right-anchored quick-settings indicator.
    pub fn items(&self, output_width: f64, ws: WorkspaceState) -> Vec<PanelItem> {
        vec![
            PanelItem {
                role: ROLE_ACTIVITIES,
                r#box: PanelBox::Left,
                rect: self.activities_rect(ws),
            },
            PanelItem {
                role: ROLE_DATE_MENU,
                r#box: PanelBox::Center,
                rect: self.date_menu_rect(output_width),
            },
            PanelItem {
                role: ROLE_QUICK_SETTINGS,
                r#box: PanelBox::Right,
                rect: self.quick_settings_rect(output_width),
            },
        ]
    }

    /// Which panel *role*, if any, sits at an output-local logical position.
    /// `output_width` is needed to place the centered dateMenu and the
    /// right-anchored quick-settings indicator.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        output_width: f64,
        ws: WorkspaceState,
    ) -> Option<&'static str> {
        if self.activities_rect(ws).contains(pos) {
            Some(ROLE_ACTIVITIES)
        } else if self.quick_settings_rect(output_width).contains(pos) {
            Some(ROLE_QUICK_SETTINGS)
        } else if self.date_menu_rect(output_width).contains(pos) {
            Some(ROLE_DATE_MENU)
        } else {
            None
        }
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        ws: WorkspaceState,
        position: f64,
        icons: &IconCache,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let scale = output.current_scale().fractional_scale();
        let width = output_size(output).w;
        let width_px: i32 = to_physical_precise_round(scale, width);
        let width_px = width_px.max(1);
        let Some(scale_key) = NotNan::new(scale).ok() else {
            return Vec::new();
        };

        let mut cache = self.cache.borrow_mut();

        // The cached textures belong to one renderer context; drop them all if it changed.
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::with_capacity(4);

        // The right-box status icons sit on top of the bar. Elements are pushed
        // first-topmost (the list is consumed in reverse). The workspace dots are now
        // drawn into the bar texture itself (rounded rects), not composited separately.
        self.qs_indicator_elements(
            renderer,
            &mut cache,
            scale,
            scale_key,
            width,
            &mut elements,
            icons,
        );

        // The bar chrome (opaque background, button containers, workspace dots, clock).
        // Button container state (hover/active) invalidates the bar cache on change, so
        // the structural key can stay content-only.
        let containers = self.button_containers(width, ws);

        // While a workspace switch animates, `position` is fractional and the dots morph
        // every frame; while a button-container fill fades, the pill alpha changes every
        // frame — in either case draw the bar fresh and skip the cache (keyed on the
        // integer active index, not the animated state). The switch / fill animations
        // already drive the per-frame redraws. At rest, cache as usual, drawing with the
        // exact integer index so the cached texture always matches its key.
        let animating =
            (position - position.round()).abs() > 1e-6 || self.are_animations_ongoing();
        let bar_texture = if animating {
            match draw_bar_texture(
                renderer,
                scale,
                width_px,
                &self.clock_text,
                &containers,
                ws.count,
                position,
            ) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    tracing::error!("error drawing the panel bar: {err:#}");
                    None
                }
            }
        } else {
            let bar_key = (scale_key, width_px, ws.count, ws.active());
            #[allow(clippy::map_entry)]
            if !cache.textures.contains_key(&bar_key) {
                match draw_bar_texture(
                    renderer,
                    scale,
                    width_px,
                    &self.clock_text,
                    &containers,
                    ws.count,
                    ws.active() as f64,
                ) {
                    Ok(texture) => {
                        cache.textures.insert(bar_key, texture);
                    }
                    Err(err) => {
                        tracing::error!("error drawing the panel bar: {err:#}");
                        return elements;
                    }
                }
            }
            cache.textures.get(&bar_key).cloned()
        };

        if let Some(texture) = bar_texture {
            // The whole bar is opaque, so let the compositor skip drawing behind it.
            let opaque = vec![Rectangle::from_size(texture.size())];
            let buffer =
                TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, opaque);
            elements.push(TextureRenderElement::from_texture_buffer(
                buffer,
                Point::from((0., 0.)),
                1.,
                None,
                None,
                Kind::Unspecified,
            ));
        }

        elements
    }

    /// Push the right-box quick-settings status icons onto `elements`, laid out in
    /// a right-anchored cluster and composited on top of the bar. Each icon is
    /// resolved from its candidate list and uploaded once per (scale, name).
    #[allow(clippy::too_many_arguments)]
    fn qs_indicator_elements(
        &self,
        renderer: &mut VulkanRenderer,
        cache: &mut BarCache,
        scale: f64,
        scale_key: NotNan<f64>,
        output_width: f64,
        elements: &mut Vec<TextureRenderElement<VkTexture>>,
        icons: &IconCache,
    ) {
        let rect = self.quick_settings_rect(output_width);
        let mut x = rect.loc.x + INDICATOR_H_PADDING;
        for candidates in qs_indicator_icons(self.toggles, &self.system_status) {
            // Resolve the first candidate that rasterizes, then cache its upload.
            let Some((name, buffer)) = candidates.iter().find_map(|name| {
                icons
                    .buffer(name, QS_ICON, scale, TEXT)
                    .map(|b| (name.to_string(), b))
            }) else {
                x += QS_ICON + QS_ICON_GAP;
                continue;
            };
            let key = (scale_key, name);
            #[allow(clippy::map_entry)]
            if !cache.qs_icons.contains_key(&key) {
                match TextureBuffer::from_memory_buffer(renderer, &buffer) {
                    Ok(tb) => {
                        cache.qs_icons.insert(key.clone(), tb);
                    }
                    Err(err) => {
                        tracing::error!("error uploading a quick-settings indicator icon: {err:#}");
                        x += QS_ICON + QS_ICON_GAP;
                        continue;
                    }
                }
            }
            if let Some(tb) = cache.qs_icons.get(&key) {
                let logical = tb.logical_size();
                let location = Point::from((x, (PANEL_HEIGHT - logical.h) / 2.));
                elements.push(TextureRenderElement::from_texture_buffer(
                    tb.clone(),
                    location,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            x += QS_ICON + QS_ICON_GAP;
        }
    }
}


/// Seconds until the next wall-clock minute boundary (1..=60), so the clock
/// tick can align to when the displayed minute actually changes.
pub fn secs_until_next_minute() -> u64 {
    // SAFETY: read the static tm immediately and copy the one field out.
    let sec = unsafe {
        let now = libc::time(null_mut());
        let tm = libc::localtime(&now);
        if tm.is_null() {
            0
        } else {
            (*tm).tm_sec
        }
    };
    (60 - i64::from(sec)).clamp(1, 60) as u64
}

/// The `strftime` format string for a clock label, assembled the way
/// gnome-shell's `GnomeDesktop.WallClock` does from the interface keys: an
/// optional weekday and date prefix, then the 12h/24h time with optional seconds.
fn strftime_format(fmt: ClockFormat) -> &'static str {
    match (
        fmt.show_weekday,
        fmt.show_date,
        fmt.hour24,
        fmt.show_seconds,
    ) {
        // 24-hour
        (false, false, true, false) => "%H:%M",
        (false, false, true, true) => "%H:%M:%S",
        (true, false, true, false) => "%a %H:%M",
        (true, false, true, true) => "%a %H:%M:%S",
        (false, true, true, false) => "%b %-e %H:%M",
        (false, true, true, true) => "%b %-e %H:%M:%S",
        (true, true, true, false) => "%a %b %-e %H:%M",
        (true, true, true, true) => "%a %b %-e %H:%M:%S",
        // 12-hour (%-l drops the leading space on the hour)
        (false, false, false, false) => "%-l:%M %p",
        (false, false, false, true) => "%-l:%M:%S %p",
        (true, false, false, false) => "%a %-l:%M %p",
        (true, false, false, true) => "%a %-l:%M:%S %p",
        (false, true, false, false) => "%b %-e %-l:%M %p",
        (false, true, false, true) => "%b %-e %-l:%M:%S %p",
        (true, true, false, false) => "%a %b %-e %-l:%M %p",
        (true, true, false, true) => "%a %b %-e %-l:%M:%S %p",
    }
}

/// Format epoch seconds as a local clock label per `fmt`, via locale-aware
/// `strftime` (like GNOME's WallClock).
fn format_clock(now: libc::time_t, fmt: ClockFormat) -> String {
    // SAFETY: localtime returns a pointer into a static buffer; we pass it
    // straight to strftime before any other libc time call touches it.
    unsafe {
        let tm = libc::localtime(&now);
        if tm.is_null() {
            return String::new();
        }
        let Ok(format) = std::ffi::CString::new(strftime_format(fmt)) else {
            return String::new();
        };
        let mut buf = [0u8; 128];
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            format.as_ptr(),
            tm,
        );
        // strftime returns 0 on overflow (never for these short labels).
        String::from_utf8_lossy(&buf[..n]).trim().to_string()
    }
}

/// Interpolate `a`→`b` by `t` (gnome-shell's `Util.lerp`).
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Draw the workspace dots into the bar frame as rounded rects, continuously
/// expanded around the active workspace so the row morphs smoothly while switching.
///
/// `position` is the live (fractional) active-workspace index — gnome-shell's
/// `WorkspacesAdjustment.value` — so at rest it's the integer active index and while a
/// switch animates it slides between two indices. Each dot's `expansion`
/// (`panel.js WorkspaceIndicators._updateExpansion`) is `clamp(1 - |i - position|, 0, 1)`,
/// driving, exactly as gnome-shell's `WorkspaceDot`: opacity `lerp(0.5, 1, e)`, dot
/// scale `lerp(0.75, 1, e)`, and an allocation-width factor `lerp(1, widthMultiplier, e)`.
/// The active dot (e=1) is a wide full-opacity pill; a distant one (e=0) a small
/// half-opacity circle; mid-switch the two straddled dots are each half-grown.
///
/// The per-dot allocation widths always sum to a constant (the two straddled dots'
/// expansions sum to 1), so the whole row — and thus `indicator_logical_width`, the hit
/// rect, and the button pill — stay the same width throughout the slide. Laid out at the
/// left, vertically centered; fully rounded (`$forced_circular_radius`). Drawn over the
/// opaque bar background, so the texture stays fully opaque.
fn draw_workspace_dots(
    frame: &mut VulkanFrame,
    scale: f64,
    count: usize,
    position: f64,
    full: Rectangle<i32, Physical>,
) -> anyhow::Result<()> {
    if count == 0 {
        return Ok(());
    }
    let mult = width_multiplier(count);
    let band_cy = PANEL_HEIGHT / 2.;

    let mut x = INDICATOR_H_PADDING; // logical left edge of the current slot
    for i in 0..count {
        let expansion = (1. - (i as f64 - position).abs()).clamp(0., 1.);
        let dot_scale = lerp(INACTIVE_DOT_SCALE, 1., expansion);
        let width_factor = lerp(1., mult, expansion);
        let opacity = lerp(INACTIVE_DOT_OPACITY, 1., expansion) as f32;

        // Allocation slot (constant total across the row); the visible dot is the
        // slot scaled by `dot_scale` about its center (gnome-shell's `scaleX`/`scaleY`).
        let slot_w = DOT_DIAMETER * width_factor;
        let draw_w = slot_w * dot_scale;
        let draw_h = DOT_DIAMETER * dot_scale;
        let slot_cx = x + slot_w / 2.;
        let rect = Rectangle::new(
            Point::<i32, Physical>::from((
                to_physical_precise_round::<i32>(scale, slot_cx - draw_w / 2.),
                to_physical_precise_round::<i32>(scale, band_cy - draw_h / 2.),
            )),
            Size::<i32, Physical>::from((
                to_physical_precise_round::<i32>(scale, draw_w).max(1),
                to_physical_precise_round::<i32>(scale, draw_h).max(1),
            )),
        );
        // Half the physical height clamps to a full circle (small dot) or stadium (pill).
        let radius = rect.size.h as f32 / 2.;
        frame.render_rounded_rect([1., 1., 1., opacity], radius, rect, &[full])?;
        x += slot_w + DOT_SPACING;
    }
    Ok(())
}

/// Draw the bar chrome into an offscreen [`VkTexture`]: clear the opaque
/// background, paint the rounded hover/active button containers, then the
/// workspace dots and the centered clock glyph run. The returned texture is
/// `SHADER_READ_ONLY` (sampleable) so the caller can composite it directly. The
/// right-box status icons are composited separately, on top.
fn draw_bar_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    width_px: i32,
    clock: &str,
    containers: &[(Rectangle<f64, Logical>, [f32; 4])],
    count: usize,
    position: f64,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("panel::draw_bar_texture");

    let width_px = width_px.max(1);
    let height_px: i32 = to_physical_precise_round(scale, PANEL_HEIGHT);
    let height_px = height_px.max(1);
    let px = (FONT_PX * scale) as f32;

    // The dateMenu clock draws bold, like GNOME's `panel_button` (font-weight: bold).
    let clock_run = renderer.build_glyph_run_weighted(clock, px, true)?;

    // Center the clock horizontally by its *advance* box, not its ink. gnome-shell's
    // WallClock uses tabular figures, so the advance width is constant as the seconds
    // tick and the label never shifts; centering on the ink (whose left edge/width
    // wobble per digit) makes the whole run jitter left/right each second. Our
    // SansSerif digits are tabular too, so an advance-centered origin is rock-steady.
    // Vertical stays ink-centered (the ink height is stable across digits).
    let advance_w = niri_vk::text::measure_line_width_weighted(clock, px, true).round() as i32;
    let (_c_ix, c_iy, _c_iw, c_ih) = clock_run.ink_bounds();
    let c_origin =
        Point::<i32, Physical>::from(((width_px - advance_w) / 2, (height_px - c_ih) / 2 - c_iy));

    let size = Size::<i32, Physical>::from((width_px, height_px));
    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((width_px, height_px)),
    )?;

    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;
        let full = Rectangle::from_size(size);

        frame.clear(Color32F::from(BAR_BG), &[full])?;
        // The rounded button containers (hover/active), behind the button content.
        for (rect, color) in containers {
            let phys = Rectangle::new(
                Point::<i32, Physical>::from((
                    to_physical_precise_round(scale, rect.loc.x),
                    to_physical_precise_round(scale, rect.loc.y),
                )),
                Size::<i32, Physical>::from((
                    to_physical_precise_round::<i32>(scale, rect.size.w).max(1),
                    to_physical_precise_round::<i32>(scale, rect.size.h).max(1),
                )),
            );
            // Half the physical height clamps the SDF to a stadium (fully rounded pill).
            let radius = phys.size.h as f32 / 2.;
            frame.render_rounded_rect(*color, radius, phys, &[full])?;
        }
        draw_workspace_dots(&mut frame, scale, count, position, full)?;
        frame.render_glyphs(&clock_run, c_origin, TEXT, full, &[full])?;
        // finish() submits and fence-waits synchronously, so the sync point is already signaled.
        let _sync = frame.finish()?;
    }

    // The bar is sampled by its own render element; transition it to shader-read.
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::ExportMem;

    use super::*;

    /// A panel with a fresh animation clock + default config, for tests that don't
    /// exercise the container fades.
    fn test_panel() -> Panel {
        Panel::new(Clock::default(), Rc::new(RefCell::new(Config::default())))
    }

    /// The indicator button widens as workspaces are added (structural — no GPU).
    #[test]
    fn indicator_width_grows_with_workspace_count() {
        let w2 = indicator_logical_width(2);
        let w3 = indicator_logical_width(3);
        let w6 = indicator_logical_width(6);
        assert!(w3 > w2, "3 workspaces should be wider than 2: {w3} vs {w2}");
        assert!(w6 > w3, "6 workspaces should be wider than 3: {w6} vs {w3}");

        let panel = test_panel();
        let r2 = panel.activities_rect(WorkspaceState {
            count: 2,
            active: 0,
        });
        let r4 = panel.activities_rect(WorkspaceState {
            count: 4,
            active: 1,
        });
        assert!(r4.size.w > r2.size.w);
        // A click at the left edge lands on the indicator; the far right does not.
        assert_eq!(
            panel.hit_test(
                Point::from((4., 10.)),
                1920.,
                WorkspaceState {
                    count: 3,
                    active: 1
                }
            ),
            Some(ROLE_ACTIVITIES)
        );
        assert_eq!(
            panel.hit_test(
                Point::from((10_000., 10.)),
                1920.,
                WorkspaceState {
                    count: 3,
                    active: 1
                }
            ),
            None
        );
    }

    /// The right-box indicator shows the live network+battery cluster when it has
    /// status, and falls back to the single anchor icon when empty (so it's always
    /// present/clickable). The populated cluster is wider.
    #[test]
    fn qs_indicator_cluster_or_anchor_fallback() {
        use crate::system_status::{BatteryStatus, NetworkStatus, SystemStatus};

        let toggles = QuickToggles::default();

        // Empty status → the single anchor fallback.
        let empty = SystemStatus::default();
        let icons = qs_indicator_icons(toggles, &empty);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0][0], QS_ANCHOR_ICONS[0]);

        // Wired + battery → network then battery, no anchor.
        let status = SystemStatus {
            network: NetworkStatus::Wired,
            battery: Some(BatteryStatus {
                icon_name: "battery-level-90-symbolic".to_string(),
                percentage: 90.,
            }),
        };
        let icons = qs_indicator_icons(toggles, &status);
        assert_eq!(icons.len(), 2);
        assert_eq!(icons[0][0], "network-wired-symbolic");
        assert_eq!(icons[1][0], "battery-level-90-symbolic");
        assert!(
            !icons.iter().any(|c| c[0] == QS_ANCHOR_ICONS[0]),
            "no anchor icon once the cluster is populated"
        );

        // The populated cluster is wider than the anchor fallback.
        let base = test_panel().quick_settings_rect(1920.).size.w;
        let mut panel = test_panel();
        panel.set_system_status(status);
        let wide = panel.quick_settings_rect(1920.).size.w;
        assert!(
            wide > base,
            "the cluster ({wide}) should be wider than the anchor fallback ({base})"
        );
    }

    /// The panel exposes both roles in their boxes (extension-representable model).
    #[test]
    fn items_expose_roles_and_boxes() {
        let panel = test_panel();
        let items = panel.items(
            1920.,
            WorkspaceState {
                count: 3,
                active: 1,
            },
        );
        let activities = items.iter().find(|i| i.role == ROLE_ACTIVITIES).unwrap();
        let date = items.iter().find(|i| i.role == ROLE_DATE_MENU).unwrap();
        assert_eq!(activities.r#box, PanelBox::Left);
        assert_eq!(date.r#box, PanelBox::Center);
        // The clock is roughly centered on the output.
        let center = date.rect.loc.x + date.rect.size.w / 2.;
        assert!((center - 960.).abs() < 1.);
    }

    /// The dots are now drawn straight into the bar offscreen with `render_rounded_rect`:
    /// in the vertically-centered band, the active pill paints a wide run of full-opacity
    /// white while an inactive dot is dimmer (half opacity). Restricted to the left dot
    /// region so the centered clock glyphs don't pollute the count. Skips with no device.
    #[test]
    fn draw_bar_texture_paints_workspace_dots() {
        use smithay::backend::renderer::{ExportMem, Texture as _};

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draw_bar_texture_paints_workspace_dots: no Vulkan device ({e})");
                return;
            }
        };
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        let scale = 2.0;
        let width_px = to_physical_precise_round::<i32>(scale, 400.);
        let mut tex = draw_bar_texture(&mut vk, scale, width_px, "12:34", &[], ws.count, 1.)
            .expect("bar texture");
        let size = tex.size();

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // Sample the band row (vertical center) over just the left dot region — the
        // dots live within `indicator_logical_width`, far from the centered clock.
        let w = size.w as usize;
        let band_y = (size.h / 2) as usize;
        let left = to_physical_precise_round::<i32>(scale, indicator_logical_width(ws.count) + 4.)
            .clamp(1, size.w) as usize;
        let row = &pixels[band_y * w * 4..band_y * w * 4 + left * 4];

        let bright_cols = row
            .chunks_exact(4)
            .filter(|p| p[0] > 200 && p[1] > 200 && p[2] > 200)
            .count();
        assert!(
            bright_cols >= (DOT_DIAMETER * scale) as usize,
            "expected a wide bright active pill, only {bright_cols} bright columns"
        );
        // A dim (half-opacity white over the dark bar) inactive dot is also present.
        let dim = row.chunks_exact(4).any(|p| p[0] > 80 && p[0] <= 200);
        assert!(dim, "expected dimmer inactive dots (half-opacity)");
    }

    /// The dot row's total allocation width is invariant to the (fractional) switch
    /// position: the two straddled dots' expansions always sum to 1, so the row — and
    /// hence `indicator_logical_width`, the button hit rect, and its pill — never resize
    /// mid-switch. Pins that invariant (pure, the property the animation relies on).
    #[test]
    fn dot_row_width_is_invariant_during_switch() {
        let count = 4;
        let mult = width_multiplier(count);
        let slot_sum = |p: f64| {
            (0..count)
                .map(|i| {
                    let e = (1. - (i as f64 - p).abs()).clamp(0., 1.);
                    DOT_DIAMETER * lerp(1., mult, e)
                })
                .sum::<f64>()
        };
        let rest = slot_sum(0.);
        for p in [0., 0.25, 0.5, 0.75, 1., 1.5, 2.5, 3.] {
            assert!(
                (slot_sum(p) - rest).abs() < 1e-9,
                "dot row width changed at position {p}: {} vs {rest}",
                slot_sum(p)
            );
        }
        // And it equals the dots term of `indicator_logical_width` (one full-width pill).
        let expected = (count as f64 - 1.) * DOT_DIAMETER + DOT_DIAMETER * mult;
        assert!((rest - expected).abs() < 1e-9, "{rest} vs {expected}");
    }

    /// Mid-switch (a fractional position), the two straddled dots are each only
    /// partially expanded — so the peak brightness in the dot region is the ~0.75-opacity
    /// of a half-grown dot, strictly dimmer than the full-white active pill at rest. Pins
    /// that the fractional path actually morphs the dots. Skips with no device.
    #[test]
    fn draw_bar_texture_dots_morph_during_switch() {
        use smithay::backend::renderer::{ExportMem, Texture as _};

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draw_bar_texture_dots_morph_during_switch: no Vulkan device ({e})");
                return;
            }
        };
        let scale = 2.0;
        let width_px = to_physical_precise_round::<i32>(scale, 400.);

        // Peak red over the band row across just the two-dot region (far from the clock).
        let peak = |vk: &mut VulkanRenderer, position: f64| -> u8 {
            let mut tex = draw_bar_texture(vk, scale, width_px, "12:34", &[], 2, position)
                .expect("bar texture");
            let size = tex.size();
            let fb = vk.bind(&mut tex).expect("bind for readback");
            let region = Rectangle::<i32, BufferCoord>::from_size(size);
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
            let w = size.w as usize;
            let band_y = (size.h / 2) as usize;
            let right = to_physical_precise_round::<i32>(scale, indicator_logical_width(2))
                .clamp(1, size.w) as usize;
            pixels[band_y * w * 4..band_y * w * 4 + right * 4]
                .chunks_exact(4)
                .map(|p| p[0])
                .max()
                .unwrap_or(0)
        };

        let rest = peak(&mut vk, 0.); // dot 0 fully active → full white
        let mid = peak(&mut vk, 0.5); // both dots half-expanded → ~0.75 opacity, no full pillar
        assert!(rest > 240, "resting active dot should be full white, peak {rest}");
        assert!(
            (150..=235).contains(&mid),
            "mid-switch dots should be partial opacity, peak {mid}"
        );
        assert!(
            mid < rest,
            "mid-switch peak {mid} must be dimmer than the resting active pill {rest}"
        );
    }

    /// The clock is centered on its advance box (see `draw_bar_texture`), which is
    /// constant across ticks only because the panel font's digits are tabular — that's
    /// what keeps the label from jittering left/right as the seconds change. Pins that
    /// invariant: if SansSerif ever resolves to a font with proportional digits this
    /// fails, flagging that advance-centering alone would no longer be steady.
    #[test]
    fn clock_advance_width_is_stable_across_seconds() {
        let px = FONT_PX as f32;
        let a = niri_vk::text::measure_line_width("12:34:56", px);
        let b = niri_vk::text::measure_line_width("12:34:07", px);
        let c = niri_vk::text::measure_line_width("18:88:88", px);
        assert_eq!(a, b, "clock width must not depend on the digits (tabular figures)");
        assert_eq!(a, c, "clock width must not depend on the digits (tabular figures)");
    }

    /// The clock's `strftime` format is assembled from the interface keys the
    /// same way GNOME's WallClock does (`dateMenu.js`). Locale-independent.
    #[test]
    fn clock_strftime_format_matches_the_interface_keys() {
        let f = |hour24, wd, date, sec| {
            strftime_format(ClockFormat {
                hour24,
                show_weekday: wd,
                show_date: date,
                show_seconds: sec,
            })
        };
        assert_eq!(f(true, false, false, false), "%H:%M");
        assert_eq!(f(true, true, false, false), "%a %H:%M");
        assert_eq!(f(true, false, false, true), "%H:%M:%S");
        assert_eq!(f(true, true, true, true), "%a %b %-e %H:%M:%S");
        assert_eq!(f(false, false, false, false), "%-l:%M %p");
        assert_eq!(f(false, true, true, false), "%a %b %-e %-l:%M %p");
    }

    /// The rendered label reflects the format: seconds add a field, and a weekday
    /// or date prefix turns the leading digit into a letter. TZ/locale-robust.
    #[test]
    fn clock_label_reflects_the_format() {
        let base = ClockFormat {
            hour24: true,
            show_weekday: false,
            show_date: false,
            show_seconds: false,
        };
        let hhmm = format_clock(0, base);
        assert_eq!(hhmm.len(), 5, "expected HH:MM, got {hhmm:?}");
        assert_eq!(hhmm.as_bytes()[2], b':', "expected HH:MM, got {hhmm:?}");

        let with_secs = format_clock(
            0,
            ClockFormat {
                show_seconds: true,
                ..base
            },
        );
        assert_eq!(
            with_secs.matches(':').count(),
            2,
            "seconds must add a field, got {with_secs:?}"
        );

        let with_weekday = format_clock(
            0,
            ClockFormat {
                show_weekday: true,
                ..base
            },
        );
        assert!(
            with_weekday
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic()),
            "a weekday prefix must lead with a letter, got {with_weekday:?}"
        );

        let with_date = format_clock(
            0,
            ClockFormat {
                show_date: true,
                ..base
            },
        );
        assert!(
            with_date.len() > hhmm.len(),
            "a date prefix must lengthen the label, got {with_date:?}"
        );
    }

    /// Showing seconds tightens the clock tick to one second; otherwise it waits
    /// for the minute boundary.
    #[test]
    fn clock_tick_interval_tightens_with_seconds() {
        let mut panel = test_panel(); // default format shows no seconds
        let minute = panel.clock_tick_interval();
        assert!(
            minute > Duration::ZERO && minute <= Duration::from_secs(60),
            "the minute tick must land on the next minute boundary, got {minute:?}"
        );
        panel.set_clock_format(ClockFormat {
            hour24: true,
            show_weekday: false,
            show_date: false,
            show_seconds: true,
        });
        assert_eq!(panel.clock_tick_interval(), Duration::from_secs(1));
    }

    /// Drive the GPU bar into an offscreen and read it back: an opaque dark
    /// background, the active Activities container pill on the left, and bright
    /// clock glyph ink. Skips cleanly with no device.
    #[test]
    fn draws_a_bar_with_glyph_coverage() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_a_bar_with_glyph_coverage: no Vulkan device ({e})");
                return;
            }
        };

        let width_px = 400;
        let height_px = PANEL_HEIGHT as i32;
        let ws = WorkspaceState {
            count: 3,
            active: 0,
        };
        // Overview open → the Activities button is active, so its container pill is drawn.
        // Advance the clock past the 150ms fade so the pill is at its full target alpha.
        let mut clock = Clock::default();
        let mut panel = Panel::new(clock.clone(), Rc::new(RefCell::new(Config::default())));
        panel.set_overview_open(true);
        clock.set_unadjusted(clock.now_unadjusted() + Duration::from_millis(500));
        let containers = panel.button_containers(width_px as f64, ws);
        let mut tex =
            draw_bar_texture(&mut vk, 1., width_px, "12:34", &containers, ws.count, ws.active as f64)
                .expect("bar texture");

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((width_px, height_px)));
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let px_at = |x: i32, y: i32| -> [u8; 4] {
            let i = ((y * width_px + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };

        // A pixel deep in the right half (away from any text) is the opaque dark bar.
        let bg = px_at(width_px - 4, height_px / 2);
        assert_eq!(bg[3], 255, "the bar must be opaque, got {bg:?}");
        assert!(
            bg[0] < 40 && bg[1] < 40 && bg[2] < 40,
            "bar bg not dark: {bg:?}"
        );

        // The active Activities container fills the left pill with grey (white α0.28
        // over black ≈ 71). Sampled above the dot band, inside the inset pill, so it's
        // container fill, not a workspace dot and not the (transparent) screen-edge margin.
        let hl = px_at(17, 6);
        assert!(
            hl[3] == 255 && hl[0] > 45 && hl[0] < 100 && hl[0] == hl[1] && hl[1] == hl[2],
            "expected the active container grey inside the pill, got {hl:?}",
        );

        // Bright glyph ink somewhere (the clock text).
        let bright = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 40, "expected visible glyph ink, got {bright}");
    }
}
