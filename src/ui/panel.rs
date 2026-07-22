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
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Color32F, ContextId, Frame as _, Renderer, Texture};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::audio::{AudioStatus, MicStatus};
use crate::gnome::{ClockFormat, QuickToggles};
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::system_status::{self, SystemStatus};
use crate::ui::widget::{self, TextShaper, TextStyle};
use crate::utils::{output_size, to_physical_precise_round};

/// Logical height of the panel. GNOME's is `2.2em` at an `11pt` base font,
/// i.e. ~32px at scale 1 (`gnome-shell-sass/widgets/_panel.scss`).
pub const PANEL_HEIGHT: f64 = 32.;

/// Panel font size. The clock draws at GNOME's `panel_button` base of 11pt
/// (`_drawing.scss`), bold. Shaping routes `FONT_PT` through [`TextShaper`]; `FONT_PX`
/// is its logical px, kept for the advance-width measure that centers the clock.
const FONT_PT: f64 = 11.;
const FONT_PX: f64 = crate::ui::pt_to_px(FONT_PT);

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

/// The dateMenu messages-indicator dot: `message-indicator-symbolic` at
/// `$scalable_icon_size` (`_panel.scss:92-94`), sitting AFTER the clock with a
/// size-matched invisible pad BEFORE it, so the clock stays centered
/// (`js/ui/dateMenu.js:871-883`). `MESSAGES_INDICATOR_SPACING` is the
/// `.clock-display-box` spacing between the box children (`_panel.scss:159-160`).
const MESSAGES_INDICATOR_ICON: f64 = 16.;
const MESSAGES_INDICATOR_SPACING: f64 = 2.;

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
/// Role of the standalone screen-recording indicator (GNOME's `screenRecording`,
/// leftmost of the right box — `sessionMode.js:99`).
pub const ROLE_SCREEN_RECORDING: &str = "screenRecording";
/// Role of the keyboard input-source indicator (GNOME's `keyboard`, `sessionMode.js:99`;
/// `InputSourceIndicator` `js/ui/status/keyboard.js:874`) — a short xkb-layout label shown
/// only when more than one layout is configured.
pub const ROLE_KEYBOARD: &str = "keyboard";

/// Right-box role order, mirroring `js/ui/sessionMode.js:99`. The remaining unbuilt
/// standalone indicators are commented out; adding one is a new entry here plus a
/// presence/width case in [`Panel::right_box_role_width`]. quickSettings anchors the
/// right edge; earlier roles stack to its left in this order.
const RIGHT_BOX_ORDER: &[&str] = &[
    ROLE_SCREEN_RECORDING,
    // screenSharing, dwellClick, a11y,
    ROLE_KEYBOARD,
    ROLE_QUICK_SETTINGS,
];

/// Right-box status-indicator icon size and inter-icon gap, logical px.
const QS_ICON: f64 = 16.;
const QS_ICON_GAP: f64 = 4.;

/// The screen-recording indicator's stop glyph and its filled-pill color
/// (`$recording_indicator_color` = `$red_4` = `#c01c28`, `_panel.scss:5`).
/// GNOME uses `screencast-stop-symbolic`, but that glyph ships bundled inside
/// gnome-shell's gresource (`data/icons/`), not in Adwaita/hicolor, so our IconCache
/// can't resolve it. Try it first (for hosts that do carry it), then fall back to
/// Adwaita's `media-playback-stop-symbolic` (a filled stop square) so the pill is
/// never iconless. First name that resolves wins.
const SCREENCAST_STOP_ICONS: &[&str] =
    &["screencast-stop-symbolic", "media-playback-stop-symbolic"];
const R1_ICON: f64 = 16.;
/// Label↔icon gap inside the recording pill: GNOME's `.screen-recording-indicator`
/// `StBoxLayout { spacing: $scaled_padding }` = 6px (`_panel.scss:64-66`, `_common.scss:57`).
const R1_SPACING: f64 = 6.;
const R1_BG: [f32; 4] = [
    0xc0 as f32 / 255.,
    0x1c as f32 / 255.,
    0x28 as f32 / 255.,
    1.,
];

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
/// Mic privacy icon (GNOME's `InputIndicator`): the sensitivity glyph while an unmuted app records,
/// the muted glyph when the source is muted. `audio-input-microphone-symbolic` is the
/// widely-shipped fallback if `microphone-sensitivity-*` is absent on a host.
const QS_MIC_ICONS: &[&str] = &[
    "microphone-sensitivity-high-symbolic",
    "audio-input-microphone-symbolic",
];
const QS_MIC_MUTED_ICONS: &[&str] = &[
    "microphone-sensitivity-muted-symbolic",
    "audio-input-microphone-symbolic",
];
/// GNOME's privacy-indicator tint (`$orange_3`, #ff7800), applied to the mic icon while an unmuted
/// app is recording (a muted mic drops the tint — no privacy concern).
const PRIVACY_ORANGE: [f32; 4] = [1., 0x78 as f32 / 255., 0., 1.];

/// Pack an RGBA tint into a hashable key byte-quad for the `qs_icons` upload cache.
fn color_key(c: [f32; 4]) -> [u8; 4] {
    [
        (c[0] * 255.) as u8,
        (c[1] * 255.) as u8,
        (c[2] * 255.) as u8,
        (c[3] * 255.) as u8,
    ]
}

/// The mic privacy indicator's icon candidates + tint, or `None` when nothing is recording. Shown
/// (tinted orange) while an unmuted app captures; muted → the muted glyph, untinted.
fn mic_indicator_icon(mic: MicStatus) -> Option<(Vec<String>, [f32; 4])> {
    if !mic.recording {
        return None;
    }
    let owned = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if mic.muted {
        Some((owned(QS_MIC_MUTED_ICONS), TEXT))
    } else {
        Some((owned(QS_MIC_ICONS), PRIVACY_ORANGE))
    }
}

/// The candidate icon-name lists for the quick-settings indicator, left-to-right:
/// active toggle touches (DND / Night Light), then the live system cluster
/// (network, then battery in the corner, like GNOME). Each entry is a candidate
/// list; the first name that resolves in the theme is drawn. Falls back to the
/// anchor icon so the cluster is never empty.
fn qs_indicator_icons(
    toggles: QuickToggles,
    status: &SystemStatus,
    audio: Option<AudioStatus>,
    mic: MicStatus,
) -> Vec<(Vec<String>, [f32; 4])> {
    let owned = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let mut v: Vec<(Vec<String>, [f32; 4])> = Vec::new();
    if toggles.do_not_disturb {
        v.push((owned(QS_DND_ICONS), TEXT));
    }
    if toggles.night_light {
        v.push((owned(QS_NIGHT_ICONS), TEXT));
    }
    // The network and airplane indicators are independent siblings in GNOME (`panel.js` adds
    // `_network` then `_rfkill`; `network.js` has no airplane input), so they can show together —
    // e.g. a wired connection stays up under airplane mode and shows *both* icons. The rfkill
    // indicator is visible when `show && active` (`rfkill.js` Indicator). Airplane only kills the
    // radios, so a *wireless* machine reads Offline under it; GNOME hides its disconnected network
    // indicator, so we suppress just the Offline network icon (never Wired/Wireless) when airplane
    // is on, then append the airplane icon after the network slot (GNOME's order).
    let airplane_on = status.airplane.show && status.airplane.active;
    if let Some(candidates) = system_status::network_icon(status.network) {
        if !(airplane_on && matches!(status.network, system_status::NetworkStatus::Offline)) {
            v.push((owned(candidates), TEXT));
        }
    }
    if airplane_on {
        v.push((owned(system_status::airplane_icon()), TEXT));
    }
    if let Some(audio) = audio {
        v.push((vec![crate::audio::volume_icon(&audio).to_string()], TEXT));
    }
    // Power profile, shown only while active (not Balanced) — gnome-shell binds the rfkill-style
    // indicator's `visible` to the toggle's `checked` (`powerProfiles.js`). Sits just before the
    // battery (GNOME adds `_powerProfiles` right before `_system`, `panel.js`).
    if status.power.show && status.power.is_active() {
        v.push((vec![status.power.icon().to_string()], TEXT));
    }
    if let Some(battery) = &status.battery {
        v.push((system_status::battery_icon(battery), TEXT));
    }
    // The mic privacy icon leads the cluster (GNOME inserts privacy indicators at the front,
    // `panel.js`), tinted orange while recording unmuted.
    if let Some(mic_icon) = mic_indicator_icon(mic) {
        v.insert(0, mic_icon);
    }
    if v.is_empty() {
        v.push((owned(QS_ANCHOR_ICONS), TEXT));
    }
    v
}

/// Logical width of the right-box quick-settings indicator (padding + icons +
/// gaps). Depends on how many status icons are currently shown.
fn qs_indicator_width(
    toggles: QuickToggles,
    status: &SystemStatus,
    audio: Option<AudioStatus>,
    mic: MicStatus,
) -> f64 {
    let n = qs_indicator_icons(toggles, status, audio, mic).len() as f64;
    2. * INDICATOR_H_PADDING + n * QS_ICON + (n - 1.) * QS_ICON_GAP
}

/// A live screen recording as the panel sees it: when it started (monotonic, for the
/// elapsed label) and the current `M:SS` string (recomputed on each 1 s tick).
struct Recording {
    started: Duration,
    label: String,
}

/// Format elapsed seconds as GNOME's `ScreenRecordingIndicator` label — `'%d:%02d'`,
/// minutes unbounded (`remoteAccess.js:103-105`).
fn format_recording(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
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
    /// Uploaded quick-settings indicator icons, keyed by (scale, resolved name, tint). The tint is
    /// part of the key because the mic privacy icon uploads orange while the rest are white — the
    /// same name at two colors must not collide.
    qs_icons: HashMap<(NotNan<f64>, String, [u8; 4]), TextureBuffer<VkTexture>>,
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
    /// Default-sink audio state (from the PipeWire watcher); its speaker icon sits
    /// in the status cluster between network and battery.
    audio: Option<AudioStatus>,
    /// Microphone activity (from the PipeWire watcher); its privacy icon leads the status cluster
    /// while an app is recording. Default = not recording.
    mic: MicStatus,
    /// Active screen recording, if any (mirrored from the screencast ledger). Drives
    /// the standalone `screenRecording` indicator — a red pill with the `M:SS`
    /// elapsed label and a stop glyph, leftmost in the right box.
    recording: Option<Recording>,
    /// The active keyboard-layout short label (e.g. "us"/"br"), or `None` when fewer than two
    /// layouts are configured. Drives the `keyboard` right-box indicator (GNOME's
    /// `InputSourceIndicator`); computed by the compositor from xkb state.
    keyboard_layout: Option<String>,
    /// The dateMenu unread-messages dot (GNOME's `MessagesIndicator`): shown when
    /// `show-banners && unseen − queued > 0` (`js/ui/dateMenu.js:787-798`). The
    /// compositor recomputes it from the notification store; a size-matched pad
    /// keeps the clock centered whether it's shown or not.
    messages_indicator: bool,

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
            audio: None,
            mic: MicStatus::default(),
            recording: None,
            keyboard_layout: None,
            messages_indicator: false,
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

    /// Adopt the live default-sink audio state (from the PipeWire watcher). Returns
    /// whether it changed, so the caller can queue a redraw; the cluster's speaker
    /// icon (and possibly its width) may differ.
    pub fn set_audio(&mut self, audio: Option<AudioStatus>) -> bool {
        if audio == self.audio {
            return false;
        }
        self.audio = audio;
        self.cache.borrow_mut().clear();
        true
    }

    /// Adopt the live microphone activity (from the PipeWire watcher). Returns whether it changed,
    /// so the caller can queue a redraw; the cluster's mic privacy icon (presence/tint) and width
    /// may differ. Clearing the cache drops any stale-tinted mic texture.
    pub fn set_mic(&mut self, mic: MicStatus) -> bool {
        if mic == self.mic {
            return false;
        }
        self.mic = mic;
        self.cache.borrow_mut().clear();
        true
    }

    /// Adopt the current screen-recording state: `Some(started)` (monotonic start of
    /// the earliest recording) shows the indicator; `None` hides it. Returns whether
    /// it changed, so the caller can queue a redraw. On a fresh recording the label
    /// starts at the current elapsed (`0:00`).
    pub fn set_recording(&mut self, started: Option<Duration>) -> bool {
        if self.recording.as_ref().map(|r| r.started) == started {
            return false;
        }
        self.recording = started.map(|started| {
            let elapsed = self.clock.now_unadjusted().saturating_sub(started);
            Recording {
                started,
                label: format_recording(elapsed.as_secs()),
            }
        });
        self.cache.borrow_mut().clear();
        true
    }

    /// Recompute the recording's `M:SS` label from the elapsed time. Returns whether
    /// the displayed string changed (driven by the 1 s recording tick).
    pub fn update_recording_label(&mut self) -> bool {
        let Some(rec) = &self.recording else {
            return false;
        };
        let elapsed = self.clock.now_unadjusted().saturating_sub(rec.started);
        let label = format_recording(elapsed.as_secs());
        if label == rec.label {
            return false;
        }
        self.recording.as_mut().unwrap().label = label;
        self.cache.borrow_mut().clear();
        true
    }

    /// Adopt the active keyboard-layout short label (`Some("us")` shows the indicator, `None` hides
    /// it — the compositor passes `None` when fewer than two layouts are configured). Returns
    /// whether it changed, so the caller can queue a redraw; the indicator's presence and the
    /// right box's width may differ.
    pub fn set_keyboard_layout(&mut self, label: Option<String>) -> bool {
        if label == self.keyboard_layout {
            return false;
        }
        self.keyboard_layout = label;
        self.cache.borrow_mut().clear();
        true
    }

    /// The current recording label (for tests / introspection), or `None` when idle.
    pub fn recording_label(&self) -> Option<&str> {
        self.recording.as_ref().map(|r| r.label.as_str())
    }

    /// Show/hide the dateMenu unread-messages dot (the compositor computes
    /// `show-banners && unseen − queued > 0`). Returns whether it changed so the
    /// caller can queue a redraw. The dot composites on top of the bar (from the
    /// icon cache), so this doesn't invalidate the bar texture.
    pub fn set_messages_indicator(&mut self, visible: bool) -> bool {
        if visible == self.messages_indicator {
            return false;
        }
        self.messages_indicator = visible;
        true
    }

    /// Whether the dateMenu messages dot is currently shown (tests/introspection).
    pub fn messages_indicator_visible(&self) -> bool {
        self.messages_indicator
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
        let clock_w =
            niri_vk::text::measure_line_width_weighted(&self.clock_text, FONT_PX as f32, true);
        let w = clock_w + H_PADDING * 2.;
        Rectangle::new(
            Point::from(((output_width - w) / 2., 0.)),
            Size::from((w, PANEL_HEIGHT)),
        )
    }

    /// The messages-indicator dot's rect (logical), or `None` when hidden: the
    /// 16px icon sitting `MESSAGES_INDICATOR_SPACING` right of the CLOCK PILL —
    /// GNOME measures the `.clock-display-box` spacing from the `.clock`
    /// element's edge, and the lit pill is the `.clock` background
    /// (`js/ui/dateMenu.js:880-883`, `_panel.scss:81-86,159-160`). The pill is
    /// `date_menu_rect` inset by `BTN_MARGIN_X` (see [`container_rect`]).
    fn messages_indicator_rect(&self, output_width: f64) -> Option<Rectangle<f64, Logical>> {
        if !self.messages_indicator {
            return None;
        }
        let clock = self.date_menu_rect(output_width);
        let pill_right = clock.loc.x + clock.size.w - BTN_MARGIN_X;
        Some(Rectangle::new(
            Point::from((
                pill_right + MESSAGES_INDICATOR_SPACING,
                (PANEL_HEIGHT - MESSAGES_INDICATOR_ICON) / 2.,
            )),
            Size::from((MESSAGES_INDICATOR_ICON, MESSAGES_INDICATOR_ICON)),
        ))
    }

    /// The dateMenu's full clickable extent: the clock button, plus the
    /// messages-indicator dot and its size-matched leading pad when the dot is
    /// shown (GNOME's whole `clock-display-box` is the button, with the pill only
    /// on the clock — `js/ui/dateMenu.js:871-886`). The clock stays centered
    /// because the pad mirrors the dot, so only this hit rect widens.
    fn date_menu_hit_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        let clock = self.date_menu_rect(output_width);
        if !self.messages_indicator {
            return clock;
        }
        // Extend to cover the dot (which sits past the clock's right edge), and
        // mirror that on the left so the clickable box stays centered.
        let dot = self.messages_indicator_rect(output_width).unwrap();
        let ext = (dot.loc.x + dot.size.w) - (clock.loc.x + clock.size.w);
        Rectangle::new(
            Point::from((clock.loc.x - ext, clock.loc.y)),
            Size::from((clock.size.w + 2. * ext, clock.size.h)),
        )
    }

    /// The quick-settings status indicator rect: the icon cluster plus a padding
    /// on each side, right-anchored on the output. Its width tracks how many
    /// status icons (toggles + live network/battery) are currently shown.
    pub fn quick_settings_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        // quickSettings anchors the right edge and is always present.
        self.right_box_rect(ROLE_QUICK_SETTINGS, output_width)
            .expect("quick settings is always present in the right box")
    }

    /// The keyboard input-source indicator's rect, or `None` when it's hidden
    /// (fewer than two layouts). Anchors the input-source popover.
    pub fn keyboard_rect(&self, output_width: f64) -> Option<Rectangle<f64, Logical>> {
        self.right_box_rect(ROLE_KEYBOARD, output_width)
    }

    /// Logical width of the screen-recording indicator pill: padding + the `M:SS`
    /// label + gap + stop icon + padding. Zero when not recording.
    fn recording_width(&self) -> f64 {
        let Some(rec) = &self.recording else {
            return 0.;
        };
        let label_w = niri_vk::text::measure_line_width_weighted(&rec.label, FONT_PX as f32, true);
        2. * INDICATOR_H_PADDING + label_w + R1_SPACING + R1_ICON
    }

    /// Logical width of the keyboard input-source indicator: padding + the short layout
    /// label + padding (a plain panel button, like the clock). Zero when hidden.
    fn keyboard_width(&self) -> f64 {
        let Some(label) = &self.keyboard_layout else {
            return 0.;
        };
        let label_w = niri_vk::text::measure_line_width_weighted(label, FONT_PX as f32, true);
        2. * INDICATOR_H_PADDING + label_w
    }

    /// The logical width a right-box role currently occupies, `0` when the role is absent
    /// (quickSettings is always present, the others come and go). The single source of
    /// truth for right-box presence, folded by [`Self::right_box_rect`] into placement.
    fn right_box_role_width(&self, role: &str) -> f64 {
        match role {
            ROLE_QUICK_SETTINGS => {
                qs_indicator_width(self.toggles, &self.system_status, self.audio, self.mic)
            }
            ROLE_SCREEN_RECORDING => self.recording_width(),
            ROLE_KEYBOARD => self.keyboard_width(),
            _ => 0.,
        }
    }

    /// The screen-recording indicator rect. Only meaningful while recording (a zero-width
    /// rect at the next indicator's left edge otherwise); callers guard on [`Self::is_recording`].
    pub fn screen_recording_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        self.right_box_rect(ROLE_SCREEN_RECORDING, output_width)
            .unwrap_or_else(|| {
                // Not recording: fall back to a zero-width rect anchored where the pill would
                // start (immediately left of whatever right-box roles are present).
                let mut right = output_width;
                for &role in RIGHT_BOX_ORDER.iter().rev() {
                    if role == ROLE_SCREEN_RECORDING {
                        break;
                    }
                    right -= self.right_box_role_width(role);
                }
                Rectangle::new(Point::from((right, 0.)), Size::from((0., PANEL_HEIGHT)))
            })
    }

    /// Whether the screen-recording indicator is currently shown.
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// The rect of a right-box role while it is currently present, else `None`. The one
    /// place a right-box indicator's geometry is decided: fold [`RIGHT_BOX_ORDER`] right-to-left
    /// from the output edge, giving each present role (nonzero width) the next slot leftward.
    /// [`Self::items`] and [`Self::hit_test`] iterate the same order, so a new indicator is one
    /// entry there plus one arm in [`Self::right_box_role_width`].
    fn right_box_rect(&self, role: &str, output_width: f64) -> Option<Rectangle<f64, Logical>> {
        let mut right = output_width;
        for &r in RIGHT_BOX_ORDER.iter().rev() {
            let w = self.right_box_role_width(r);
            if w <= 0. {
                continue; // absent
            }
            let rect = Rectangle::new(Point::from((right - w, 0.)), Size::from((w, PANEL_HEIGHT)));
            if r == role {
                return Some(rect);
            }
            right -= w;
        }
        None
    }

    /// The panel's items with their current rectangles, for introspection and the
    /// (deferred) extension host. `output_width` is the output's logical width, used
    /// to place the centered clock and the right-anchored quick-settings indicator.
    pub fn items(&self, output_width: f64, ws: WorkspaceState) -> Vec<PanelItem> {
        let mut items = vec![
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
        ];
        // The right box, in `sessionMode.js:99` order — each role present only when it
        // has a rect (screenRecording comes and goes with the recording, like GNOME
        // hiding the actor).
        for &role in RIGHT_BOX_ORDER {
            if let Some(rect) = self.right_box_rect(role, output_width) {
                items.push(PanelItem {
                    role,
                    r#box: PanelBox::Right,
                    rect,
                });
            }
        }
        items
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
        } else if let Some(role) = RIGHT_BOX_ORDER.iter().copied().find(|&role| {
            self.right_box_rect(role, output_width)
                .is_some_and(|rect| rect.contains(pos))
        }) {
            Some(role)
        } else if self.date_menu_hit_rect(output_width).contains(pos) {
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

        // The screen-recording indicator's stop glyph, composited on top of its red pill
        // (which is drawn into the bar below). Same upload/caching as the QS cluster icons.
        if self.is_recording() {
            let r1 = self.screen_recording_rect(width);
            let icon_x = r1.loc.x + r1.size.w - INDICATOR_H_PADDING - R1_ICON;
            if let Some((name, buffer)) = SCREENCAST_STOP_ICONS
                .iter()
                .find_map(|n| icons.buffer(n, R1_ICON, scale, TEXT).map(|b| (*n, b)))
            {
                let key = (scale_key, name.to_string(), color_key(TEXT));
                #[allow(clippy::map_entry)]
                if !cache.qs_icons.contains_key(&key) {
                    if let Ok(tb) = TextureBuffer::from_memory_buffer(renderer, &buffer) {
                        cache.qs_icons.insert(key.clone(), tb);
                    } else {
                        tracing::error!("error uploading the screen-recording stop icon");
                    }
                }
                if let Some(tb) = cache.qs_icons.get(&key) {
                    let logical = tb.logical_size();
                    let location = Point::from((icon_x, (PANEL_HEIGHT - logical.h) / 2.));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb.clone(),
                        location,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
            }
        }

        // The dateMenu messages-indicator dot (`message-indicator-symbolic`),
        // composited on top of the bar after the clock, in the panel fg. Same
        // upload/caching as the recording stop glyph and the QS cluster icons.
        if let Some(rect) = self.messages_indicator_rect(width) {
            if let Some(buffer) = icons.buffer(
                "message-indicator-symbolic",
                MESSAGES_INDICATOR_ICON,
                scale,
                TEXT,
            ) {
                let key = (
                    scale_key,
                    "message-indicator-symbolic".to_string(),
                    color_key(TEXT),
                );
                #[allow(clippy::map_entry)]
                if !cache.qs_icons.contains_key(&key) {
                    if let Ok(tb) = TextureBuffer::from_memory_buffer(renderer, &buffer) {
                        cache.qs_icons.insert(key.clone(), tb);
                    } else {
                        tracing::error!("error uploading the messages-indicator icon");
                    }
                }
                if let Some(tb) = cache.qs_icons.get(&key) {
                    let logical = tb.logical_size();
                    let location = Point::from((
                        rect.loc.x + (rect.size.w - logical.w) / 2.,
                        (PANEL_HEIGHT - logical.h) / 2.,
                    ));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb.clone(),
                        location,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
            }
        }

        // The bar chrome (opaque background, button containers, workspace dots, clock).
        // Button container state (hover/active) invalidates the bar cache on change, so
        // the structural key can stay content-only.
        let mut containers = self.button_containers(width, ws);
        // The screen-recording pill is an always-filled red container (not a hover fade),
        // drawn by the same rounded-rect path; its M:SS label is drawn on top in the bar.
        let recording_label = self.recording.as_ref().map(|rec| {
            let r1 = self.screen_recording_rect(width);
            containers.push((container_rect(r1), R1_BG));
            (rec.label.clone(), r1.loc.x + INDICATOR_H_PADDING)
        });
        // The keyboard input-source label — a plain (transparent) panel button, no fill
        // container this slice (hover/checked is deferred with its source menu). Left-aligned
        // at the button padding, like the recording pill's label.
        let keyboard_label = self
            .keyboard_layout
            .as_ref()
            .zip(self.right_box_rect(ROLE_KEYBOARD, width))
            .map(|(label, kb)| (label.clone(), kb.loc.x + INDICATOR_H_PADDING));

        // While a workspace switch animates, `position` is fractional and the dots morph
        // every frame; while a button-container fill fades, the pill alpha changes every
        // frame — in either case draw the bar fresh and skip the cache (keyed on the
        // integer active index, not the animated state). The switch / fill animations
        // already drive the per-frame redraws. At rest, cache as usual, drawing with the
        // exact integer index so the cached texture always matches its key.
        let animating = (position - position.round()).abs() > 1e-6 || self.are_animations_ongoing();
        let bar_texture = if animating {
            match draw_bar_texture(
                renderer,
                scale,
                width_px,
                &self.clock_text,
                &containers,
                ws.count,
                position,
                recording_label.as_ref().map(|(s, x)| (s.as_str(), *x)),
                keyboard_label.as_ref().map(|(s, x)| (s.as_str(), *x)),
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
                    recording_label.as_ref().map(|(s, x)| (s.as_str(), *x)),
                    keyboard_label.as_ref().map(|(s, x)| (s.as_str(), *x)),
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
        for (candidates, color) in
            qs_indicator_icons(self.toggles, &self.system_status, self.audio, self.mic)
        {
            // Resolve the first candidate that rasterizes (in its tint), then cache its upload.
            let Some((name, buffer)) = candidates.iter().find_map(|name| {
                icons
                    .buffer(name, QS_ICON, scale, color)
                    .map(|b| (name.to_string(), b))
            }) else {
                x += QS_ICON + QS_ICON_GAP;
                continue;
            };
            let key = (scale_key, name, color_key(color));
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
#[allow(clippy::too_many_arguments)]
fn draw_bar_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    width_px: i32,
    clock: &str,
    containers: &[(Rectangle<f64, Logical>, [f32; 4])],
    count: usize,
    position: f64,
    recording_label: Option<(&str, f64)>,
    keyboard_label: Option<(&str, f64)>,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("panel::draw_bar_texture");

    let width_px = width_px.max(1);
    let height_px: i32 = to_physical_precise_round(scale, PANEL_HEIGHT);
    let height_px = height_px.max(1);
    let px = (FONT_PX * scale) as f32;

    // Shape every run up front (needs `&mut renderer`, before the bake frame opens). `TextShaper`
    // owns the pt → physical-px multiply; the clock draws bold, like GNOME's `panel_button`.
    let bold = TextStyle::new(FONT_PT).bold();
    let (clock_run, recording, keyboard) = {
        let mut shaper = TextShaper::new(renderer, scale);
        let clock_run = shaper.shape(clock, bold)?;
        // Bold, left-aligned at its button/pill padding, ink-centered vertically like the clock.
        let shape_label = |shaper: &mut TextShaper, label: &str, lx: f64| -> anyhow::Result<_> {
            let run = shaper.shape(label, bold)?;
            let (_ix, iy, _iw, ih) = run.ink_bounds();
            let origin = Point::<i32, Physical>::from((
                to_physical_precise_round(scale, lx),
                (height_px - ih) / 2 - iy,
            ));
            Ok((run, origin))
        };
        let recording = recording_label
            .map(|(label, lx)| shape_label(&mut shaper, label, lx))
            .transpose()?;
        let keyboard = keyboard_label
            .map(|(label, lx)| shape_label(&mut shaper, label, lx))
            .transpose()?;
        (clock_run, recording, keyboard)
    };

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

    widget::bake_uncached_sized(renderer, size, |frame| {
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
        draw_workspace_dots(frame, scale, count, position, full)?;
        frame.render_glyphs(clock_run.run(), c_origin, TEXT, full, &[full])?;
        // The screen-recording pill's M:SS label, over its red container.
        if let Some((run, origin)) = &recording {
            frame.render_glyphs(run.run(), *origin, TEXT, full, &[full])?;
        }
        // The keyboard input-source short label.
        if let Some((run, origin)) = &keyboard {
            frame.render_glyphs(run.run(), *origin, TEXT, full, &[full])?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem};
    use smithay::utils::Buffer as BufferCoord;

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

        let no_mic = MicStatus::default();

        // Empty status → the single anchor fallback.
        let empty = SystemStatus::default();
        let icons = qs_indicator_icons(toggles, &empty, None, no_mic);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].0[0], QS_ANCHOR_ICONS[0]);

        // Wired + battery → network then battery, no anchor.
        let status = SystemStatus {
            network: NetworkStatus::Wired,
            battery: Some(BatteryStatus {
                icon_name: "battery-level-90-symbolic".to_string(),
                percentage: 90.,
            }),
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &status, None, no_mic);
        assert_eq!(icons.len(), 2);
        assert_eq!(icons[0].0[0], "network-wired-symbolic");
        assert_eq!(icons[1].0[0], "battery-level-90-symbolic");
        assert!(
            !icons.iter().any(|c| c.0[0] == QS_ANCHOR_ICONS[0]),
            "no anchor icon once the cluster is populated"
        );

        // With audio, the speaker icon sits between network and battery.
        let audio = Some(AudioStatus {
            volume: 0.5,
            muted: false,
        });
        let icons = qs_indicator_icons(toggles, &status, audio, no_mic);
        assert_eq!(icons.len(), 3);
        assert_eq!(icons[0].0[0], "network-wired-symbolic");
        assert_eq!(icons[1].0[0], "audio-volume-medium-symbolic");
        assert_eq!(icons[2].0[0], "battery-level-90-symbolic");

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

    /// The airplane indicator is an independent sibling of the network one (GNOME `panel.js` adds
    /// `_network` then `_rfkill`): it appears only when `show && active`, right after the network
    /// slot, and a live wired connection keeps showing its icon alongside it. A *wireless* machine
    /// reads Offline under airplane, and GNOME hides that disconnected network indicator — so the
    /// Offline network icon (and only that one) is suppressed while airplane is on.
    #[test]
    fn airplane_icon_accompanies_network_and_suppresses_only_offline() {
        use crate::system_status::{AirplaneStatus, NetworkStatus, SystemStatus};

        let toggles = QuickToggles::default();
        let no_mic = MicStatus::default();
        let on = AirplaneStatus {
            active: true,
            show: true,
        };

        // Wired + airplane on → both the wired icon and the airplane icon, network first.
        let wired = SystemStatus {
            network: NetworkStatus::Wired,
            airplane: on,
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &wired, None, no_mic);
        assert_eq!(icons.len(), 2);
        assert_eq!(icons[0].0[0], "network-wired-symbolic");
        assert_eq!(icons[1].0[0], "airplane-mode-symbolic");

        // Wireless machine goes Offline under airplane → the disconnected network icon is hidden,
        // leaving just the airplane icon.
        let offline = SystemStatus {
            network: NetworkStatus::Offline,
            airplane: on,
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &offline, None, no_mic);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].0[0], "airplane-mode-symbolic");

        // `show` without `active` (rfkill hardware present, airplane off) → no airplane icon.
        let off = SystemStatus {
            network: NetworkStatus::Wired,
            airplane: AirplaneStatus {
                active: false,
                show: true,
            },
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &off, None, no_mic);
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].0[0], "network-wired-symbolic");
    }

    /// The power-profile indicator appears (with the active profile's icon, before the battery)
    /// only while the daemon is present AND the active profile isn't Balanced — GNOME binds it to
    /// the toggle's `checked`. Balanced, or an absent daemon, shows nothing.
    #[test]
    fn power_profile_icon_shows_only_when_not_balanced() {
        use crate::system_status::{
            BatteryStatus, KnownProfile, NetworkStatus, PowerProfileStatus, SystemStatus,
        };

        let toggles = QuickToggles::default();
        let no_mic = MicStatus::default();
        let battery = || {
            Some(BatteryStatus {
                icon_name: "battery-level-90-symbolic".to_string(),
                percentage: 90.,
            })
        };
        let available = || {
            vec![
                KnownProfile::Performance,
                KnownProfile::Balanced,
                KnownProfile::PowerSaver,
            ]
        };

        // Performance active → the performance icon sits between network and battery.
        let perf = SystemStatus {
            network: NetworkStatus::Wired,
            battery: battery(),
            power: PowerProfileStatus {
                active: "performance".to_string(),
                available: available(),
                show: true,
            },
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &perf, None, no_mic);
        assert_eq!(icons.len(), 3);
        assert_eq!(icons[0].0[0], "network-wired-symbolic");
        assert_eq!(icons[1].0[0], "power-profile-performance-symbolic");
        assert_eq!(icons[2].0[0], "battery-level-90-symbolic");

        // Balanced → no power-profile icon (checked is false).
        let balanced = SystemStatus {
            power: PowerProfileStatus {
                active: "balanced".to_string(),
                ..perf.power.clone()
            },
            ..perf.clone()
        };
        let icons = qs_indicator_icons(toggles, &balanced, None, no_mic);
        assert_eq!(icons.len(), 2);
        assert!(!icons.iter().any(|(c, _)| c[0].starts_with("power-profile")));

        // Daemon absent (show = false) → no icon even for a non-balanced active string.
        let hidden = SystemStatus {
            power: PowerProfileStatus {
                active: "performance".to_string(),
                available: available(),
                show: false,
            },
            ..perf
        };
        let icons = qs_indicator_icons(toggles, &hidden, None, no_mic);
        assert!(!icons.iter().any(|(c, _)| c[0].starts_with("power-profile")));
    }

    /// The mic privacy icon leads the cluster (leftmost) while recording, tinted orange when
    /// unmuted and drawn muted-untinted when the source is muted; it widens the cluster and
    /// disappears when recording stops. Pure/structural, no GPU.
    #[test]
    fn mic_privacy_icon_leads_the_cluster_when_recording() {
        use crate::system_status::{NetworkStatus, SystemStatus};

        let toggles = QuickToggles::default();
        let wired = || SystemStatus {
            network: NetworkStatus::Wired,
            battery: None,
            ..Default::default()
        };

        // Not recording → no mic icon at all.
        let icons = qs_indicator_icons(toggles, &wired(), None, MicStatus::default());
        assert!(!icons.iter().any(|(c, _)| c[0].starts_with("microphone")));

        // Recording + unmuted → mic icon FIRST, tinted orange, network still after it.
        let rec = MicStatus {
            recording: true,
            muted: false,
            ..MicStatus::default()
        };
        let icons = qs_indicator_icons(toggles, &wired(), None, rec);
        assert_eq!(icons[0].0[0], "microphone-sensitivity-high-symbolic");
        assert_eq!(icons[0].1, PRIVACY_ORANGE);
        assert!(icons.iter().any(|(c, _)| c[0] == "network-wired-symbolic"));

        // Recording + muted → the muted glyph, untinted (white, no privacy concern).
        let muted = MicStatus {
            recording: true,
            muted: true,
            ..MicStatus::default()
        };
        let icons = qs_indicator_icons(toggles, &wired(), None, muted);
        assert_eq!(icons[0].0[0], "microphone-sensitivity-muted-symbolic");
        assert_eq!(icons[0].1, TEXT);

        // Recording widens the cluster (mic ADDS to the network icon), and clearing narrows it.
        let mut panel = test_panel();
        panel.set_system_status(wired());
        let base = panel.quick_settings_rect(1920.).size.w;
        assert!(panel.set_mic(rec));
        let wide = panel.quick_settings_rect(1920.).size.w;
        assert!(
            wide > base,
            "recording widens the cluster ({wide} vs {base})"
        );
        assert!(panel.set_mic(MicStatus::default()));
        assert_eq!(panel.quick_settings_rect(1920.).size.w, base);
        assert!(!panel.set_mic(MicStatus::default()), "no-op re-set");
    }

    /// The dateMenu messages-indicator dot (`js/ui/dateMenu.js:871-886`): the
    /// setter is a no-op-detecting toggle; the dot sits right of the clock; the
    /// clock stays centered whether or not the dot shows (the pad mirrors it);
    /// and clicking the dot still opens the calendar (the button's hit rect
    /// widens to include it). Structural, no GPU.
    #[test]
    fn messages_indicator_sits_right_of_a_centered_clock() {
        let ow = 1920.;
        let mut panel = test_panel();

        // Hidden by default: no dot rect, hit rect == the bare clock rect.
        assert!(!panel.messages_indicator_visible());
        assert!(panel.messages_indicator_rect(ow).is_none());
        let clock = panel.date_menu_rect(ow);
        assert_eq!(panel.date_menu_hit_rect(ow), clock);

        // Show it: the setter reports the change once, then no-ops.
        assert!(panel.set_messages_indicator(true));
        assert!(!panel.set_messages_indicator(true), "no-op re-set");
        assert!(panel.messages_indicator_visible());

        // The clock rect is UNCHANGED — the dot doesn't push the clock off
        // center (the invisible leading pad mirrors it).
        assert_eq!(panel.date_menu_rect(ow), clock);
        let center = clock.loc.x + clock.size.w / 2.;
        assert!((center - ow / 2.).abs() < 1., "clock stays centered");

        // The dot is a 16px square 2px right of the CLOCK PILL edge (the pill is
        // the clock rect inset by BTN_MARGIN_X), matching GNOME's box spacing.
        let dot = panel.messages_indicator_rect(ow).unwrap();
        assert_eq!(dot.size.w, MESSAGES_INDICATOR_ICON);
        assert_eq!(
            dot.loc.x,
            clock.loc.x + clock.size.w - BTN_MARGIN_X + MESSAGES_INDICATOR_SPACING
        );

        // The hit rect grew symmetrically to cover the dot (and its leading
        // pad), so a click on the dot opens the calendar.
        let hit = panel.date_menu_hit_rect(ow);
        assert!(hit.contains(Point::from((dot.loc.x + 8., 10.))));
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        assert_eq!(
            panel.hit_test(Point::from((dot.loc.x + 8., 10.)), ow, ws),
            Some(ROLE_DATE_MENU),
            "clicking the dot opens the dateMenu"
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
                eprintln!(
                    "skipping draw_bar_texture_paints_workspace_dots: no Vulkan device ({e})"
                );
                return;
            }
        };
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        let scale = 2.0;
        let width_px = to_physical_precise_round::<i32>(scale, 400.);
        let mut tex = draw_bar_texture(
            &mut vk,
            scale,
            width_px,
            "12:34",
            &[],
            ws.count,
            1.,
            None,
            None,
        )
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
                eprintln!(
                    "skipping draw_bar_texture_dots_morph_during_switch: no Vulkan device ({e})"
                );
                return;
            }
        };
        let scale = 2.0;
        let width_px = to_physical_precise_round::<i32>(scale, 400.);

        // Peak red over the band row across just the two-dot region (far from the clock).
        let peak = |vk: &mut VulkanRenderer, position: f64| -> u8 {
            let mut tex =
                draw_bar_texture(vk, scale, width_px, "12:34", &[], 2, position, None, None)
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
        assert!(
            rest > 240,
            "resting active dot should be full white, peak {rest}"
        );
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
        assert_eq!(
            a, b,
            "clock width must not depend on the digits (tabular figures)"
        );
        assert_eq!(
            a, c,
            "clock width must not depend on the digits (tabular figures)"
        );
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
        let mut tex = draw_bar_texture(
            &mut vk,
            1.,
            width_px,
            "12:34",
            &containers,
            ws.count,
            ws.active as f64,
            None,
            None,
        )
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

    /// `format_recording` matches GNOME's `'%d:%02d'`: zero-padded seconds, unbounded minutes.
    #[test]
    fn recording_label_formats_minutes_and_seconds() {
        assert_eq!(format_recording(0), "0:00");
        assert_eq!(format_recording(5), "0:05");
        assert_eq!(format_recording(65), "1:05");
        assert_eq!(format_recording(600), "10:00");
        assert_eq!(format_recording(3661), "61:01");
    }

    /// The screen-recording indicator appears only while recording, as a right-box item
    /// directly left of (adjacent to) quick-settings, and hit-tests to its role.
    /// `sessionMode.js:99` order; structural, no GPU.
    #[test]
    fn screen_recording_indicator_sits_left_of_quick_settings() {
        let clock = Clock::default();
        let mut panel = Panel::new(clock.clone(), Rc::new(RefCell::new(Config::default())));
        let ws = WorkspaceState {
            count: 1,
            active: 0,
        };
        let ow = 1920.;

        // Idle: no screenRecording item, and its rect region hit-tests to nothing.
        assert!(panel
            .items(ow, ws)
            .iter()
            .all(|i| i.role != ROLE_SCREEN_RECORDING));

        // Recording (label 0:00 at the pinned now).
        let now = clock.now_unadjusted();
        assert!(panel.set_recording(Some(now)));
        assert_eq!(panel.recording_label(), Some("0:00"));

        let items = panel.items(ow, ws);
        let r1 = items
            .iter()
            .find(|i| i.role == ROLE_SCREEN_RECORDING)
            .expect("the recording indicator is present while recording");
        let qs = items
            .iter()
            .find(|i| i.role == ROLE_QUICK_SETTINGS)
            .unwrap();
        assert_eq!(r1.r#box, PanelBox::Right);
        assert!(
            r1.rect.loc.x < qs.rect.loc.x,
            "R1 is left of quick-settings"
        );
        assert!(
            (r1.rect.loc.x + r1.rect.size.w - qs.rect.loc.x).abs() < 1e-6,
            "R1 abuts quick-settings",
        );

        // A click at the indicator's center hit-tests to its role.
        let center = Point::from((r1.rect.loc.x + r1.rect.size.w / 2., 16.));
        assert_eq!(panel.hit_test(center, ow, ws), Some(ROLE_SCREEN_RECORDING));

        // Stopping hides it again.
        assert!(panel.set_recording(None));
        assert!(panel
            .items(ow, ws)
            .iter()
            .all(|i| i.role != ROLE_SCREEN_RECORDING));
    }

    /// The keyboard indicator is absent until a label is set (GNOME hides it with <2 sources),
    /// then sits directly left of quick-settings and right of the recording pill, hit-testing to
    /// its role. `sessionMode.js:99` order; structural, no GPU.
    #[test]
    fn keyboard_indicator_visibility_and_order() {
        let clock = Clock::default();
        let mut panel = Panel::new(clock.clone(), Rc::new(RefCell::new(Config::default())));
        let ws = WorkspaceState {
            count: 1,
            active: 0,
        };
        let ow = 1920.;

        // A single layout (compositor passes `None`): no keyboard item, no change on re-set.
        assert!(!panel.set_keyboard_layout(None));
        assert!(panel.items(ow, ws).iter().all(|i| i.role != ROLE_KEYBOARD));

        // Two layouts → the compositor passes the active short label; the indicator appears.
        assert!(panel.set_keyboard_layout(Some("us".into())));
        let items = panel.items(ow, ws);
        let kb = items
            .iter()
            .find(|i| i.role == ROLE_KEYBOARD)
            .expect("the keyboard indicator is present with a label");
        let qs = items
            .iter()
            .find(|i| i.role == ROLE_QUICK_SETTINGS)
            .unwrap();
        assert_eq!(kb.r#box, PanelBox::Right);
        assert!(kb.rect.loc.x < qs.rect.loc.x, "keyboard is left of QS");
        assert!(
            (kb.rect.loc.x + kb.rect.size.w - qs.rect.loc.x).abs() < 1e-6,
            "keyboard abuts quick-settings",
        );
        let center = Point::from((kb.rect.loc.x + kb.rect.size.w / 2., 16.));
        assert_eq!(panel.hit_test(center, ow, ws), Some(ROLE_KEYBOARD));

        // With a recording too, the order is screenRecording | keyboard | quickSettings.
        let now = clock.now_unadjusted();
        assert!(panel.set_recording(Some(now)));
        let items = panel.items(ow, ws);
        let rx = items
            .iter()
            .find(|i| i.role == ROLE_SCREEN_RECORDING)
            .unwrap()
            .rect;
        let kx = items.iter().find(|i| i.role == ROLE_KEYBOARD).unwrap().rect;
        assert!(rx.loc.x < kx.loc.x, "recording is left of keyboard");
        assert!(
            (rx.loc.x + rx.size.w - kx.loc.x).abs() < 1e-6,
            "recording abuts keyboard",
        );

        // A wider label widens the indicator (width tracks the measured text).
        let w1 = panel.keyboard_width();
        assert!(panel.set_keyboard_layout(Some("us2".into())));
        assert!(panel.keyboard_width() > w1, "a longer label is wider");

        // Clearing the label hides it again.
        assert!(panel.set_keyboard_layout(None));
        assert!(panel.items(ow, ws).iter().all(|i| i.role != ROLE_KEYBOARD));
    }

    /// The recording pill draws red (`#c01c28`) with its white `M:SS` label on top.
    /// Skips with no device.
    #[test]
    fn draw_bar_texture_paints_the_recording_pill() {
        use smithay::backend::renderer::{ExportMem, Texture as _};

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "skipping draw_bar_texture_paints_the_recording_pill: no Vulkan device ({e})"
                );
                return;
            }
        };
        let scale = 2.0;
        let width_px = to_physical_precise_round::<i32>(scale, 400.);

        // A red pill on the right with a label just inside its left padding.
        let pill = Rectangle::new(Point::from((300., 3.)), Size::from((90., 26.)));
        let containers = [(pill, R1_BG)];
        let mut tex = draw_bar_texture(
            &mut vk,
            scale,
            width_px,
            "12:34",
            &containers,
            3,
            1.,
            Some(("0:05", 306.)),
            None,
        )
        .expect("bar texture");
        let size = tex.size();

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // Readback is [R, G, B, A] per pixel. Scan the pill's physical column range.
        let w = size.w as usize;
        let x0 = to_physical_precise_round::<i32>(scale, 300.).max(0) as usize;
        let x1 = (to_physical_precise_round::<i32>(scale, 390.) as usize).min(w);
        let mut red = 0usize;
        let mut ink = 0usize;
        for y in 0..size.h as usize {
            for x in x0..x1 {
                let p = &pixels[(y * w + x) * 4..(y * w + x) * 4 + 4];
                if p[0] > 150 && p[1] < 90 && p[2] < 90 && p[3] > 200 {
                    red += 1;
                }
                if p[0] > 200 && p[1] > 200 && p[2] > 200 {
                    ink += 1;
                }
            }
        }
        assert!(red > 50, "expected a red recording pill, got {red} red px");
        assert!(
            ink > 5,
            "expected white M:SS label ink over the pill, got {ink} px"
        );
    }
}
