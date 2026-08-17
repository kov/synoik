// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The GNOME top panel.
//!
//! A persistent bar drawn in-compositor at the top of each output while the
//! session is in GNOME (floating) windowing mode. It draws the bar chrome, a
//! left-hand **workspace indicator** (the graphical dots that replaced GNOME's
//! old "Activities" text button — click toggles the overview, scroll switches
//! workspace) and, at the far right past the status indicators, the clock
//! (a divergence — see `RIGHT_BOX_ORDER`); the panel also reserves a top strut so
//! windows never sit under it (see `layout::workspace::compute_working_area`).
//!
//! The bar is drawn entirely on the GPU through the owned Vulkan renderer: an
//! offscreen `VkTexture` is cleared transparent, the clock glyph run
//! is drawn with the [`render_glyphs`](VulkanFrame::render_glyphs) material, and
//! the result is composited as a `TextureRenderElement` — no cairo/pango raster.
//! The bar's *background* is not in that bake at all: it is a blurred capture of the scene
//! behind the bar with a translucent dark wash over it, so the panel is see-through — a
//! deliberate divergence from gnome-shell's opaque bar, see [`BAR_BG`].
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

use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Color32F, ContextId, Renderer};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};
use synoik_config::Config;

use crate::animation::{Animation, Clock};
use crate::audio::{AudioStatus, MicStatus};
use crate::gnome::{A11ySettings, ClockFormat, QuickToggles};
use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::blur::{BlurOptions, Finish};
use crate::render_helpers::framebuffer_effect::{FramebufferEffect, FramebufferEffectElement};
use crate::render_helpers::icon::{DrawCaches, IconCache, ImageFit};
use crate::render_helpers::rounded_solid::{RoundedSolidBuffer, RoundedSolidRenderElement};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::status_notifier::ItemIcon;
use crate::synoik_render_elements;
use crate::system_status::{self, SystemStatus};
use crate::ui::widget::{self, Painter, TextShaper, TextStyle};
use crate::utils::{output_size, to_physical_precise_round};

/// Logical height of the panel: GNOME's `$panel_height: 2.2em` (`_panel.scss:10,16`).
///
/// The `em` is the **realized** base font's em ([`crate::ui::em`]), which is why this is a
/// function. It was a `35.` constant, measured off a real session and documented as proof that
/// GNOME's structural ems resolve against the mixin's 16px reference base rather than the font em
/// (`2.2 × 16 = 35.2`). That reading was an artifact: the session measured had `font-name` stuck
/// at `Cantarell 12`, and 12pt's em *is* 16px, so the two candidate rules gave the same answer.
/// At the default 11pt the same panel measures **32** — `2.2 × 14.667 = 32.27` — which only the
/// font-em rule predicts. See [[ui-font-family-runtime]]; both measurements now agree.
pub fn panel_height() -> f64 {
    // Rounded because St allocates the panel in whole logical px: at 11pt the theme's 32.27 is a
    // 32px panel, and every strut, work area and overview band derived from it wants that integer
    // rather than a fraction that reappears as 29.999999999999996 three layers down.
    crate::ui::em(2.2).round()
}

/// Panel font size. The clock draws at GNOME's `panel_button` base of 11pt
/// (`_drawing.scss`), bold. Shaping routes `FONT_PT` through [`TextShaper`]; `font_px()`
/// is its logical px, kept for the advance-width measure that sizes the clock button.
const FONT_PT: f64 = 11.;
fn font_px() -> f64 {
    crate::ui::pt_to_px(FONT_PT)
}

/// Base workspace-dot diameter, logical px. GNOME: `$scalable_icon_size (16) * 0.5`
/// (`gnome-shell-sass/widgets/_panel.scss`), fully rounded (`$forced_circular_radius`).
pub(crate) const DOT_DIAMETER: f64 = 8.;

/// Gap between dots, logical px (`panel.js` `WorkspaceIndicators` box `spacing`).
const DOT_SPACING: f64 = 5.;

/// Horizontal padding from the button's hit-rect edge to its content (the dot row
/// or the status-icon cluster), logical px: the panel_button `-natural-hpadding`
/// (`BTN_H_PADDING`) measured from the button edge, plus the icon's own
/// `margin: 0 $base_margin` (`_panel.scss:34`).
///
/// This is 16 either way, but *not* via the pill: measured live, a `.system-status-icon`'s
/// margin box starts exactly `BTN_H_PADDING` inside the button rect, so the pill's
/// `BTN_MARGIN_X` does not enter a content offset. Reading it as "pill inset + breathing
/// room" is what put the clock 8px short — see [`clock_h_padding`].
const INDICATOR_H_PADDING: f64 = BTN_H_PADDING + STATUS_ICON_MARGIN_X;

/// `.panel-button .system-status-icon { margin: 0 $base_margin }` (`_panel.scss:34`).
const STATUS_ICON_MARGIN_X: f64 = 4.;

/// Inactive dots are drawn at 0.75× and half-opacity (`panel.js`
/// `INACTIVE_WORKSPACE_DOT_SCALE`, `WorkspaceDot._updateVisuals`).
const INACTIVE_DOT_SCALE: f64 = 0.75;
const INACTIVE_DOT_OPACITY: f64 = 0.5;

/// Horizontal padding from the dateMenu (clock) button's hit-rect edge to the clock
/// label, logical px: the panel_button `-natural-hpadding` (`BTN_H_PADDING`) plus
/// `.clock`'s own `padding-left/right: $scaled_padding * 2` (`_panel.scss:161-164`).
///
/// The clock does *not* share the status icons' inset: an icon contributes a 4px margin
/// where the clock contributes 12px of padding, so the label sits 24px in, not 16.
/// `$scaled_padding` is `to_em(6px)`, so unlike `$base_margin` this half scales with the
/// user's font — hence [`crate::ui::em`] rather than a literal.
fn clock_h_padding() -> f64 {
    BTN_H_PADDING + 2. * crate::ui::scaled_px(6.)
}

/// The dateMenu messages-indicator dot: `message-indicator-symbolic` at
/// `$scalable_icon_size` (`_panel.scss:92-94`).
///
/// **Divergence, following the clock's** (see [`RIGHT_BOX_ORDER`]). GNOME hangs the dot off
/// the clock as a sibling in the `.clock-display-box`, with a size-matched *leading* pad so
/// the pair stays centred in the panel (`js/ui/dateMenu.js:871-883`). That pad also happens
/// to keep the clock still as the dot comes and goes — and with the clock right-anchored,
/// stillness is the only part worth keeping. Rather than reserve a slot for the dot, we put
/// it where there is already room: the clock button's own trailing `clock_h_padding()`,
/// empty by construction. So the dot costs no layout at all, the button never changes size,
/// and an arriving or dismissed notification cannot shove the clock — or every status
/// indicator left of it — sideways. It draws over the button's pill rather than beside it,
/// which GNOME never does; it reads as a trailing badge on the button.
/// **Ours, not GNOME's** (`$scalable_icon_size` is 16): the dot shares the clock button's
/// trailing padding with the label rather than having a box of its own, and at 16 it filled
/// that space wall to wall. 12 leaves [`MESSAGES_INDICATOR_GAP`] of air on both sides.
const MESSAGES_INDICATOR_ICON: f64 = 12.;

/// How far the dot's trailing edge sits in from the lit pill's, logical px — its only
/// placement rule, since the pill's rounded end is what it would otherwise collide with.
///
/// This gap and the one left of the dot trade off directly: the pill has just
/// `clock_h_padding() - BTN_MARGIN_X` (20px) of interior after the clock label's advance
/// box, which the dot and its two margins have to share. At GNOME's 16px icon there was
/// nothing left to distribute — the dot sat flush in the pill's rounded end — so the icon
/// shrank to [`MESSAGES_INDICATOR_ICON`] and both sides get this.
const MESSAGES_INDICATOR_GAP: f64 = 6.;

/// Bar background — GNOME's dark panel `$panel_bg_color` = `$dark_5` `#000000`
/// (`_colors.scss:24` / `_palette.scss:46`), straight RGBA, but **translucent**.
///
/// **Divergence (deliberate).** gnome-shell's panel is fully opaque; ours is a dark wash over a
/// blurred capture of whatever is behind it ([`BAR_BLUR`]), the way the widely-used Blur my Shell
/// extension does it. The alpha is what stands in for a brightness knob — the blur path has none —
/// so it is chosen to land the backdrop near the 0.6 multiply that keeps the white panel foreground
/// legible over an arbitrary wallpaper, the same job [`crate::ui::lock_screen::BLUR_BRIGHTNESS`]
/// does behind the lock clock.
pub(crate) const BAR_BG: [f32; 4] = [0., 0., 0., 0.4];

/// The panel's backdrop blur — a dual-Kawase pass over the mid-frame capture of the strip behind
/// the bar. Fixed rather than config-driven: there is no config file (see
/// `docs/fork/STRATEGY.md`), and these are the values the surface-level effect defaults to
/// (`synoik_config::Blur::default`).
pub(crate) const BAR_BLUR: BlurOptions = BlurOptions {
    passes: 3,
    offset: 3.,
};

/// The bar background at `overview_fade`, where 0 is the normal desktop and 1 the
/// fully-open overview. GNOME drops the panel to `background-color: transparent`
/// while the overview (or the lock/login screen) is up — `#panel:overview`,
/// `_panel.scss:98-102` — so the `#overviewGroup` fill (`$system_base_color`,
/// `_overview.scss:7-9`) runs unbroken from the top of the screen to the bottom and
/// the bar reads as part of the overview rather than a black band above it. The
/// crossfade is a plain CSS transition on the color, `$panel_transition_duration`
/// 250ms = the overview's own `ANIMATION_TIME` (`_panel.scss:10-18`), which is why
/// riding the overview progress directly reproduces it. Only the *background* fades:
/// the clock, dots and status icons stay fully opaque throughout. The blur under the wash
/// ([`BAR_BLUR`]) does not fade — see the note where it is pushed.
fn bar_bg(overview_fade: f64) -> [f32; 4] {
    let [r, g, b, a] = BAR_BG;
    [r, g, b, a * (1. - overview_fade as f32)]
}

/// Panel-button container inset from its hit rect (`_drawing.scss` `panel_button`
/// mixin): `$base_margin` (4px) horizontally so an edge button isn't glued to the
/// screen edge, and the 3px transparent border vertically. What's left is the
/// fully-rounded (`$forced_circular_radius`) pill that lights up on hover/active.
pub(crate) const BTN_MARGIN_X: f64 = 4.;
const BTN_INSET_Y: f64 = 3.;

/// Horizontal breathing room between the lit pill and the button's content, logical
/// px — gnome-shell's panel_button `-natural-hpadding` (`$base_padding * 2` = 12px,
/// `_panel.scss`). Without it the pill hugs the dots/icons; the button's content
/// padding is this plus the pill's own `BTN_MARGIN_X` edge inset.
const BTN_H_PADDING: f64 = 12.;

/// `panel_button` fill *alpha* over the dark bar (white `$fg`) — the SDF fill blends
/// over the bar background: idle 0, hover `transparentize($fg, .83)`,
/// active/`:checked` `transparentize($fg, .72)`, active+hover `transparentize($fg, .68)`.
/// The container color is white at the (animated) alpha of these.
const BTN_HOVER_A: f32 = 0.17;
const BTN_ACTIVE_A: f32 = 0.28;
const BTN_ACTIVE_HOVER_A: f32 = 0.32;

/// The three panel-button roles whose containers fade between states.
/// Every panel button that wears the shared hover/checked "pill" — the fully-rounded
/// `panel-button` state-layer wash that fades in on hover and stays lit while the
/// button's menu is up. GNOME gives every `.panel-button` this background
/// (`_panel.scss:112-113`), and each of these roles is a real `PanelMenu.Button`
/// (Activities, `dateMenu`, `quickSettings`, and the `InputSourceIndicator` keyboard
/// menu — `js/ui/status/keyboard.js:875`). The one place a role opts into the pill:
/// add it here and give [`Panel::pill_rect`] its geometry. `screenRecording` is
/// excluded — it carries its own always-on red fill rather than the state layer.
const PILL_ROLES: [&str; 5] = [
    ROLE_ACTIVITIES,
    ROLE_DATE_MENU,
    ROLE_QUICK_SETTINGS,
    ROLE_KEYBOARD,
    ROLE_A11Y,
];

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
/// Role of the clock (GNOME's `dateMenu` panel role). GNOME centres it; we put it at the
/// right end of the panel — see [`RIGHT_BOX_ORDER`].
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
/// Role of the accessibility indicator (GNOME's `a11y`, `sessionMode.js:99`; `ATIndicator`
/// `js/ui/status/accessibility.js:32`) — an `accessibility-menu-symbolic` button shown only
/// when a11y is pinned on or some a11y feature is enabled (`accessibility.js:90-97`).
pub const ROLE_A11Y: &str = "a11y";

/// Role of the app-indicator cluster — every registered StatusNotifierItem, in one slot.
///
/// **Not a GNOME role**: GNOME has no StatusNotifier support, so this name is ours (see
/// `docs/fork/status-notifier-port.md`). It is one role rather than one per item because the panel
/// addresses roles by `&'static str`; the cluster sub-hit-tests internally, exactly as
/// `quickSettings` does for its icons ([`Panel::app_indicator_at`]).
pub const ROLE_APP_INDICATORS: &str = "appIndicators";

/// Right-box role order, mirroring `js/ui/sessionMode.js:99`. The remaining unbuilt
/// standalone indicators are commented out; adding one is a new entry here plus a
/// presence/width case in [`Panel::right_box_role_width`]. The last entry anchors the
/// right edge; earlier roles stack to its left in this order.
///
/// **Divergence — the clock lives at the right end.** GNOME puts `dateMenu` alone in the
/// panel's *center* box (`js/ui/panel.js` `_centerBox`, `sessionMode.js:98`); we move it
/// into the right box, past `quickSettings`, so it anchors the screen's right corner and
/// the status cluster sits to its left. Nothing else about the button changes — same pill,
/// same padding, same calendar popover (which re-centers on the button and is clamped to
/// the screen edge by [`crate::ui::popover::PanelPopover::location`]). The center box is
/// consequently empty, so [`PanelBox::Center`] no longer has an occupant.
const RIGHT_BOX_ORDER: &[&str] = &[
    // App indicators lead the right box, so a session that gathers a dozen of them grows
    // leftward and never displaces the clock or the status cluster.
    ROLE_APP_INDICATORS,
    ROLE_SCREEN_RECORDING,
    // screenSharing, dwellClick,
    ROLE_A11Y,
    ROLE_KEYBOARD,
    ROLE_QUICK_SETTINGS,
    ROLE_DATE_MENU,
];

/// Right-box status-indicator icon size, logical px (`$scalable_icon_size`).
const QS_ICON: f64 = 16.;
/// `.panel-status-indicators-box` inter-child spacing, logical px
/// (`_panel.scss:40` `spacing: $base_margin` = 4px).
const QS_BOX_SPACING: f64 = 4.;
/// Each `.system-status-icon`'s per-side horizontal margin inside the box, logical px
/// (`_panel.scss:35` `margin: 0 $base_margin` = 4px). The box rule overrides only
/// `padding: 0` (`_panel.scss:42-44`), so this margin still applies to every icon —
/// St stacks it on top of the box spacing.
const QS_ICON_MARGIN: f64 = 4.;
/// Effective gap between adjacent status icons: box spacing plus each icon's facing
/// margin (4 + 4 + 4 = 12px), matching GNOME's rendered aggregate cluster.
const QS_ICON_GAP: f64 = QS_BOX_SPACING + 2. * QS_ICON_MARGIN;

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

/// The accessibility indicator's icon (`accessibility.js:39`). Like the screencast
/// stop glyph, `accessibility-menu-symbolic` ships inside gnome-shell's own gresource
/// (`data/icons/scalable/actions/`), not Adwaita, so our IconCache may not resolve it;
/// fall back to Adwaita's `preferences-desktop-accessibility-symbolic` so the button is
/// never iconless. First name that resolves wins.
const A11Y_ICONS: &[&str] = &[
    "accessibility-menu-symbolic",
    "preferences-desktop-accessibility-symbolic",
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

/// A revision of everything the battery bake draws, so the texture is rebuilt when — and only
/// when — the picture changes.
///
/// The fill is quantised to whole physical-ish steps rather than keyed on the raw percentage: the
/// bar is 21 logical px wide, so sub-half-percent changes cannot move it and would otherwise
/// re-bake for nothing.
fn battery_revision(look: &system_status::BatteryLook, fill: f64) -> u64 {
    let steps = (widget::Battery::fill_width(fill) * 4.).round() as u64;
    widget::Revision::new()
        .of(look.body as u8)
        .of(look.fill as u8)
        .of(look.overlay as u8)
        .of(steps)
        .done()
}

/// One slot in the quick-settings status cluster.
///
/// Almost every slot is a themed symbolic icon of [`QS_ICON`] width. The battery is not: it is a
/// self-painted [`widget::Battery`] nearly twice as wide, which is the reason the cluster walks
/// per-element widths ([`qs_slot_x`]) instead of `i * QS_ICON`.
#[derive(Debug, Clone)]
enum QsSlot {
    /// Candidate icon names (first that resolves in the theme wins) and the tint to draw it in.
    Icon(Vec<String>, [f32; 4]),
    Battery(system_status::BatteryStatus),
}

impl QsSlot {
    /// The width this slot occupies in the cluster.
    fn width(&self) -> f64 {
        match self {
            QsSlot::Icon(..) => QS_ICON,
            QsSlot::Battery(_) => widget::Battery::WIDTH,
        }
    }
}

/// The candidate icon-name lists for the quick-settings indicator, left-to-right:
/// active toggle touches (DND / Night Light), then the live system cluster
/// (network, then battery in the corner, like GNOME). Each entry is a candidate
/// list; the first name that resolves in the theme is drawn. Falls back to the
/// anchor icon so the cluster is never empty.
fn qs_indicator_slots(
    toggles: QuickToggles,
    status: &SystemStatus,
    audio: Option<AudioStatus>,
    mic: MicStatus,
) -> Vec<QsSlot> {
    let owned = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    let mut v: Vec<QsSlot> = Vec::new();
    if toggles.do_not_disturb {
        v.push(QsSlot::Icon(owned(QS_DND_ICONS), TEXT));
    }
    if toggles.night_light {
        v.push(QsSlot::Icon(owned(QS_NIGHT_ICONS), TEXT));
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
            v.push(QsSlot::Icon(owned(candidates), TEXT));
        }
    }
    // Bluetooth: visible iff any connected device (`bluetooth.js:458-464`). GNOME's indicator
    // order puts bluetooth between network and rfkill (`panel.js:351-357`), so it slots here.
    if status.bluetooth.connected_count() > 0 {
        v.push(QsSlot::Icon(owned(&["bluetooth-active-symbolic"]), TEXT));
    }
    if airplane_on {
        v.push(QsSlot::Icon(owned(system_status::airplane_icon()), TEXT));
    }
    if let Some(audio) = audio {
        v.push(QsSlot::Icon(
            vec![crate::audio::volume_icon(&audio).to_string()],
            TEXT,
        ));
    }
    // Power profile, shown only while active (not Balanced) — gnome-shell binds the rfkill-style
    // indicator's `visible` to the toggle's `checked` (`powerProfiles.js`). Sits just before the
    // battery (GNOME adds `_powerProfiles` right before `_system`, `panel.js`).
    if status.power.show && status.power.is_active() {
        v.push(QsSlot::Icon(vec![status.power.icon().to_string()], TEXT));
    }
    if let Some(battery) = &status.battery {
        v.push(QsSlot::Battery(battery.clone()));
    }
    // The mic privacy icon leads the cluster (GNOME inserts privacy indicators at the front,
    // `panel.js`), tinted orange while recording unmuted.
    if let Some(mic_icon) = mic_indicator_icon(mic) {
        v.insert(0, QsSlot::Icon(mic_icon.0, mic_icon.1));
    }
    if v.is_empty() {
        v.push(QsSlot::Icon(owned(QS_ANCHOR_ICONS), TEXT));
    }
    v
}

/// The cluster as themed icon names, one entry per slot, in cluster order.
///
/// A *slot identification* view, not a drawing one: the battery draws itself
/// ([`widget::Battery`]) and contributes the symbolic name it would have had, so ordering tests
/// and the volume slot lookup can index the cluster by icon name. Nothing renders from this.
fn qs_indicator_icons(
    toggles: QuickToggles,
    status: &SystemStatus,
    audio: Option<AudioStatus>,
    mic: MicStatus,
) -> Vec<(Vec<String>, [f32; 4])> {
    qs_indicator_slots(toggles, status, audio, mic)
        .into_iter()
        .map(|slot| match slot {
            QsSlot::Icon(candidates, color) => (candidates, color),
            QsSlot::Battery(battery) => (system_status::battery_icon(&battery), TEXT),
        })
        .collect()
}

/// The widths of `n` slots that are all plain `.system-status-icon`s — the shape every cluster
/// had before the battery indicator, and still the shape of the app-indicator cluster.
fn icon_widths(n: usize) -> Vec<f64> {
    vec![QS_ICON; n]
}

/// The x of the `i`-th slot in a right-box cluster, from the indicator rect's left edge, given
/// every slot's width. The single source of truth for the cluster's spacing: the render loop and
/// the per-slot hit test must agree, or a scroll lands on a neighbour of the icon it looks like it
/// is over.
///
/// Takes widths rather than an index-times-constant so the cluster can hold a slot that is not
/// `QS_ICON` wide — the dynamic battery indicator is a wide self-painted element sitting among
/// 16px icons (`docs/fork/battery-indicator-design.md`). Anything that walks the cluster must walk
/// these widths, never `i * QS_ICON`. Every slot is still an icon today; this is the seam.
fn qs_slot_x(widths: &[f64], rect_x: f64, i: usize) -> f64 {
    rect_x
        + INDICATOR_H_PADDING
        + QS_ICON_MARGIN
        + widths[..i].iter().map(|w| w + QS_ICON_GAP).sum::<f64>()
}

/// Logical width of a right-box cluster (padding + slots + gaps).
///
/// The box's outer slots keep their facing margin too, so the cluster carries one
/// `QS_ICON_MARGIN` of breathing room at each end inside the button padding. Shared by the
/// quick-settings cluster and the app indicators so both measure the same as
/// [`qs_slot_x`] places.
fn qs_cluster_width(widths: &[f64]) -> f64 {
    let gaps = (widths.len().max(1) - 1) as f64;
    2. * INDICATOR_H_PADDING + 2. * QS_ICON_MARGIN + widths.iter().sum::<f64>() + gaps * QS_ICON_GAP
}

/// Logical width of the right-box quick-settings indicator. Depends on how many status
/// icons are currently shown.
fn qs_indicator_width(
    toggles: QuickToggles,
    status: &SystemStatus,
    audio: Option<AudioStatus>,
    mic: MicStatus,
) -> f64 {
    qs_cluster_width(&qs_indicator_widths(toggles, status, audio, mic))
}

/// The width of each quick-settings cluster slot, in cluster order. Derived from the same
/// [`qs_indicator_slots`] walk the render loop and the hit tests use, so the three cannot drift.
fn qs_indicator_widths(
    toggles: QuickToggles,
    status: &SystemStatus,
    audio: Option<AudioStatus>,
    mic: MicStatus,
) -> Vec<f64> {
    qs_indicator_slots(toggles, status, audio, mic)
        .iter()
        .map(QsSlot::width)
        .collect()
}

/// One app indicator as the panel draws it: an id to act on and the icon name to resolve.
///
/// Deliberately not the whole [`crate::status_notifier::Indicator`] — the panel needs a name and a
/// handle, and keeping the untrusted item state out of the draw path keeps the seam honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelIndicator {
    /// The item's public id, for routing a click back to the right item.
    pub id: String,
    /// What to draw, in whichever form the client offered. [`ItemIcon::None`] keeps the slot but
    /// paints nothing.
    pub icon: ItemIcon,
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

/// Which of the bar's text labels a cached texture belongs to. Each is baked and cached
/// on its own, so one changing does not re-rasterize the others — see [`BarCache::textures`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LabelSlot {
    /// The centre clock, `PanelMenu.Button`-styled and bold.
    Clock,
    /// The screen-recording pill's `M:SS` elapsed label.
    Recording,
    /// The input-source indicator's short layout name ("us", "br").
    Keyboard,
}

/// One baked bar label, with the text it was baked from (the cache hit test) and the physical-px
/// offset from its anchor column to the texture's left edge — see [`draw_label_texture`].
struct CachedLabel {
    text: String,
    buffer: TextureBuffer<VkTexture>,
    lead_px: i32,
}

/// Cached bar textures, keyed so a content change misses. Tied to a renderer
/// context: dropped wholesale when the renderer changes.
struct BarCache {
    context: Option<ContextId<VkTexture>>,
    /// The bar's text labels, one entry per [`LabelSlot`], keyed by (scale, slot) and holding
    /// the text each was baked from. A label misses when *its own* text changes — which is the
    /// whole point of the split: the clock ticks once a minute (once a *second* with seconds
    /// shown), and it used to invalidate a bake the full width of the output.
    ///
    /// The workspace state used to be part of this key, because the dots were baked in
    /// here and the active index placed them. They are [`RoundedSolidRenderElement`]s now,
    /// so nothing in these bakes varies with the workspace at all. Neither is the background
    /// ([`bar_bg`]) — one cached bake serves the whole overview fade *and* the whole
    /// workspace switch.
    ///
    /// The cached value is the [`TextureBuffer`], not the bare texture: the buffer is what carries
    /// the element `Id`, and building a fresh one per frame from a cached texture churns that `Id`
    /// even though not a pixel changed. Damage tracking then sees the old element *gone* and a new
    /// one arrive every frame, which forces a full redraw of every framebuffer effect on the
    /// output — see [`Self::bg`] and [`Self::dots`], kept for exactly this reason.
    /// `nothing_churns_its_element_id_per_frame` is the guard.
    textures: HashMap<(NotNan<f64>, LabelSlot), CachedLabel>,
    /// The bar background. A buffer rather than a bare colour so its commit counter
    /// bumps when the colour or width changes — that is what tells damage tracking the
    /// background moved, now that it is no longer part of the chrome bake.
    ///
    /// Not dropped by `clear`/`clear_bars`: a fresh one would carry a new `Id` and force
    /// a full-panel redraw for nothing.
    bg: SolidColorBuffer,
    /// The button-container pills, keyed by `(scale, size)` with the colour in the
    /// revision — [`widget::bake`]'s own cache, so a pill is rasterized once per
    /// shape and reused.
    ///
    /// They are separate from the chrome bake for the same reason [`Self::bg`] is:
    /// their only animated property is alpha, and folding an animated alpha into a
    /// bake makes the bake miss on every frame of the fade. A pill's *shape* changes
    /// rarely (the clock's width once a minute, the indicator's with the workspace
    /// count), so the cache holds a handful of entries and is stable across a whole
    /// animation.
    pills: widget::BakeCache,
    /// One persistent identity per workspace dot, indexed by workspace. Their geometry and
    /// opacity are recomputed every frame of a switch, but the `Id` behind each has to
    /// survive across frames or damage tracking sees a brand-new element each time.
    ///
    /// Not dropped by `clear` — like [`Self::bg`], these hold no pixels, only identity.
    dots: Vec<RoundedSolidBuffer>,
    /// The dynamic battery indicator's baked body, with the `(scale, revision)` it was baked at.
    ///
    /// A [`TextureBuffer`], not a bare texture, for the same reason [`Self::textures`] is: the
    /// buffer carries the element `Id`, and rebuilding one per frame from an unchanged texture
    /// churns that `Id` and forces a full redraw of every framebuffer effect on the output.
    /// The battery is the worst possible offender for that — it is on screen at all times.
    battery: Option<CachedBattery>,
}

/// The battery indicator's baked body and the key it is valid for.
struct CachedBattery {
    /// Scale plus a revision of everything the bake depends on — see [`battery_revision`].
    key: (NotNan<f64>, u64),
    buffer: TextureBuffer<VkTexture>,
}

impl BarCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
            bg: SolidColorBuffer::default(),
            pills: widget::BakeCache::new(),
            dots: Vec::new(),
            battery: None,
        }
    }

    /// Drop the cached bar chrome (content or renderer changed). The composited status
    /// icons are not in here — they live in the shared [`IconCache`], which outlives any
    /// bar re-bake, so a hover that redraws the bar never re-uploads an icon.
    fn clear(&mut self) {
        self.textures.clear();
        // Holds a texture from the old renderer context, so it goes with them.
        self.battery = None;
    }
}

// One thing the panel contributes to a frame, front-to-back.
//
// Three variants because everything that animates is its own layer, over the one baked
// texture that does not:
//
// * `Texture` — the chrome bake (clock and labels), plus the composited icons.
// * `Solid` — the bar background, whose alpha fades as the overview opens.
// * `RoundedSolid` — the workspace dots, whose *geometry* interpolates during a switch.
//
// The reason is the same in both animated cases and it is the expensive one: a bake is a
// GPU round trip, so any animated property folded into it costs one round trip per frame
// for as long as the animation runs. An alpha can ride the element instead; a size cannot,
// which is why the dots need a real drawing primitive rather than a cached texture.
// * `Backdrop` — the blurred capture of the scene behind the bar, under everything else.
synoik_render_elements! {
    PanelElement => {
        Texture = TextureRenderElement<VkTexture>,
        Solid = SolidColorRenderElement,
        RoundedSolid = RoundedSolidRenderElement,
        Backdrop = FramebufferEffectElement,
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
    /// The app indicators currently shown, in the order the watcher registered them.
    app_indicators: Vec<PanelIndicator>,
    /// The dateMenu unread-messages dot (GNOME's `MessagesIndicator`): shown when
    /// `show-banners && unseen − queued > 0` (`js/ui/dateMenu.js:787-798`). The
    /// compositor recomputes it from the notification store. It trails the clock inside
    /// the right-anchored dateMenu box, so showing it slides the clock (and everything
    /// left of it) over — see [`MESSAGES_INDICATOR_EXTENT`].
    messages_indicator: bool,
    /// The accessibility state driving the `a11y` right-box indicator's presence
    /// (`ATIndicator._syncMenuVisibility`, `js/ui/status/accessibility.js:90-97`).
    a11y: A11ySettings,

    /// Animation clock + config, for the button-container fill fades.
    clock: Clock,
    config: Rc<RefCell<Config>>,
    /// Per-role container fill-alpha fade (keyed by role), so hover/active
    /// transitions ease over 150ms instead of snapping.
    fills: HashMap<&'static str, FillFade>,

    /// Cached GPU chrome, cleared whenever the drawn content changes.
    cache: RefCell<BarCache>,

    /// Identity of the bar's backdrop-blur element — see [`BAR_BG`]. It holds no pixels: the
    /// capture and the blur chain hang off this `Id` in whatever per-element state the render path
    /// keeps, which is per output *and* per render target ([`FramebufferEffect::new`]). One stable
    /// `Id` across frames is all this has to provide.
    backdrop: FramebufferEffect,
}

impl Panel {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        let clock_format = ClockFormat::default();
        let fills = PILL_ROLES
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
            app_indicators: Vec::new(),
            messages_indicator: false,
            a11y: A11ySettings::default(),
            clock,
            config,
            fills,
            cache: RefCell::new(BarCache::new()),
            backdrop: FramebufferEffect::new(),
        }
    }

    /// Adopt the accessibility state (from a gsettings change or a menu row click).
    /// Returns whether it changed, so the caller can queue a redraw; the indicator's
    /// presence and the right box's layout both depend on it.
    pub fn set_a11y(&mut self, a11y: A11ySettings) -> bool {
        if a11y == self.a11y {
            return false;
        }
        self.a11y = a11y;
        self.cache.borrow_mut().clear();
        true
    }

    /// The accessibility state the indicator is drawn from.
    pub fn a11y(&self) -> A11ySettings {
        self.a11y
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
        // No cache invalidation: the label's bake is keyed on its own text
        // ([`BarCache::textures`]), so this misses exactly one label-sized texture and leaves
        // the clock, the keyboard label and every composited icon alone. It ticks once a
        // second while recording — back when all three labels shared one output-width bake,
        // this line re-rasterized the entire bar at that cadence.
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
    /// caller can queue a redraw. The dot composites on top of the bar (from the icon
    /// cache) into a slot the dateMenu box reserves for it either way
    /// ([`MESSAGES_INDICATOR_EXTENT`]), so nothing in the bar texture moves and this
    /// doesn't invalidate it.
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
            // No cache invalidation — see `update_recording_label`. With seconds shown this
            // fires every second, and nothing but the clock's own texture depends on it.
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

    /// Which panel button the pointer is hovering, if any.
    #[must_use]
    pub fn hovered_role(&self) -> Option<&'static str> {
        self.hovered
    }

    /// Set which panel button the pointer is hovering (`None` when off any button).
    /// Returns whether it changed, so the caller can queue a redraw. Nothing is
    /// invalidated: a hover only moves a container fill, which is its own element
    /// ([`pill_element`]) — see [`Self::retarget_fills`].
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
    /// target — gnome-shell's `panel_button` 150ms transition.
    ///
    /// Deliberately does **not** touch [`BarCache`]. The fills used to be painted into the
    /// bar bake, so retargeting one had to invalidate it; they are their own elements now
    /// ([`pill_element`], fed by [`Self::button_containers`]) and nothing the bake draws
    /// varies with a fill. The stale invalidation was expensive in exactly the place it is
    /// least affordable: `activities_checked` flips on every overview open *and* close, so
    /// each transition threw away every label bake and the battery, and re-baked them —
    /// 1.1–7.2ms of GPU round trips landing on a frame that is already animating.
    /// `opening_the_overview_rebakes_no_panel_chrome` is the guard.
    fn retarget_fills(&mut self) {
        let config = self.config.borrow().animations.panel_popover_open_close.0;
        for role in PILL_ROLES {
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

    /// A pill-capable button's hit rect for this frame, or `None` when the role is
    /// currently absent (e.g. the keyboard indicator with a single layout). The one
    /// place the shared pill maps a [`PILL_ROLES`] entry to its geometry, so hover /
    /// checked highlighting and hit-testing stay in lock-step for every button.
    fn pill_rect(
        &self,
        role: &str,
        output_width: f64,
        ws: WorkspaceState,
    ) -> Option<Rectangle<f64, Logical>> {
        match role {
            ROLE_ACTIVITIES => Some(self.activities_rect(ws)),
            // The dateMenu's right-box slot spans the messages dot too, but only the
            // `.clock` wears the pill (`js/ui/dateMenu.js:880-886`).
            ROLE_DATE_MENU => Some(self.date_menu_rect(output_width)),
            // quickSettings (always present) and keyboard (present with >1 layout) both
            // live in the right box, so their geometry comes from the same folder.
            _ => self.right_box_rect(role, output_width),
        }
    }

    /// The rounded containers to paint behind the buttons this frame, each a
    /// (pill rect, fill color) — only for buttons with a non-zero (animated) fill.
    /// The same building block (`render_rounded_rect`) for every [`PILL_ROLES`] entry,
    /// so they're consistent. `output_width` places the centered/right-anchored buttons.
    fn button_containers(
        &self,
        output_width: f64,
        ws: WorkspaceState,
    ) -> Vec<(Rectangle<f64, Logical>, [f32; 4])> {
        let mut v = Vec::new();
        for &role in &PILL_ROLES {
            let alpha = self.fills.get(role).map_or(0., FillFade::value);
            if alpha <= 0.001 {
                continue;
            }
            // A role can be mid-fade the frame its indicator disappears (keyboard drops
            // to one layout); with no rect there is nothing to light, so skip it.
            if let Some(rect) = self.pill_rect(role, output_width, ws) {
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
            Size::from((indicator_logical_width(ws.count), panel_height())),
        )
    }

    /// Logical width of the dateMenu button: the shaped clock label plus a padding on
    /// each side. The messages dot costs nothing here — it draws *inside* the trailing
    /// padding (see [`Self::messages_indicator_rect`]), which is what keeps the button a
    /// fixed size whether or not there is anything unread.
    fn date_menu_width(&self) -> f64 {
        let clock_w =
            synoik_vk::text::measure_line_width_weighted(&self.clock_text, font_px() as f32, true);
        clock_w + clock_h_padding() * 2.
    }

    /// The dateMenu (clock) button rect — its right-box slot ([`RIGHT_BOX_ORDER`]), so the
    /// same fold that places quickSettings places the clock. This is the whole button:
    /// what it draws, what wears the pill, and what a click hits, dot or no dot.
    /// `output_width` is the output's logical width.
    pub fn date_menu_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        self.right_box_rect(ROLE_DATE_MENU, output_width)
            .expect("the dateMenu is always present in the right box")
    }

    /// The messages-indicator dot's rect (logical), or `None` when hidden: the 16px icon
    /// centred in the clock button's TRAILING PADDING — the `clock_h_padding()` of empty
    /// space that already exists between the label and the button's right edge. That is
    /// the divergence (see [`MESSAGES_INDICATOR_ICON`]): the dot costs no layout, so it
    /// can appear and disappear without moving the clock.
    pub(crate) fn messages_indicator_rect(
        &self,
        output_width: f64,
    ) -> Option<Rectangle<f64, Logical>> {
        if !self.messages_indicator {
            return None;
        }
        let clock = self.date_menu_rect(output_width);
        let right = clock.loc.x + clock.size.w;
        Some(Rectangle::new(
            Point::from((
                right - BTN_MARGIN_X - MESSAGES_INDICATOR_GAP - MESSAGES_INDICATOR_ICON,
                (panel_height() - MESSAGES_INDICATOR_ICON) / 2.,
            )),
            Size::from((MESSAGES_INDICATOR_ICON, MESSAGES_INDICATOR_ICON)),
        ))
    }

    /// The quick-settings status indicator rect: the icon cluster plus a padding
    /// on each side, right-anchored on the output. Its width tracks how many
    /// status icons (toggles + live network/battery) are currently shown.
    /// How the battery indicator currently reads, or `None` with no battery. The observable
    /// state the corpus asserts against — the tint roles and overlay, not pixels.
    pub fn battery_look(&self) -> Option<system_status::BatteryLook> {
        self.system_status
            .battery
            .as_ref()
            .map(system_status::battery_look)
    }

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
        let label_w =
            synoik_vk::text::measure_line_width_weighted(&rec.label, font_px() as f32, true);
        2. * INDICATOR_H_PADDING + label_w + R1_SPACING + R1_ICON
    }

    /// Logical width of the keyboard input-source indicator: padding + the short layout
    /// label + padding (a plain panel button, like the clock). Zero when hidden.
    fn keyboard_width(&self) -> f64 {
        let Some(label) = &self.keyboard_layout else {
            return 0.;
        };
        let label_w = synoik_vk::text::measure_line_width_weighted(label, font_px() as f32, true);
        2. * INDICATOR_H_PADDING + label_w
    }

    /// The accessibility indicator's rect, or `None` when it's hidden (nothing enabled
    /// and not pinned on — `accessibility.js:90-97`).
    pub fn a11y_rect(&self, output_width: f64) -> Option<Rectangle<f64, Logical>> {
        self.right_box_rect(ROLE_A11Y, output_width)
    }

    /// Logical width of the accessibility indicator: padding + one `.system-status-icon`
    /// + padding. Zero when hidden.
    fn a11y_width(&self) -> f64 {
        if !self.a11y.indicator_visible() {
            return 0.;
        }
        2. * INDICATOR_H_PADDING + QS_ICON
    }

    /// The app-indicator cluster's rect, or `None` when no indicator is shown.
    pub fn app_indicators_rect(&self, output_width: f64) -> Option<Rectangle<f64, Logical>> {
        self.right_box_rect(ROLE_APP_INDICATORS, output_width)
    }

    /// Logical width of the app-indicator cluster: the same padding-plus-icons arithmetic the
    /// quick-settings cluster uses, so one indicator is exactly as wide as one status icon.
    /// Zero when there is nothing to show.
    fn app_indicators_width(&self) -> f64 {
        if self.app_indicators.is_empty() {
            return 0.;
        }
        qs_cluster_width(&icon_widths(self.app_indicators.len()))
    }

    /// The rect of the `i`-th app indicator, for hit-testing a click at one of several icons
    /// sharing the cluster's slot. Mirrors [`Self::volume_indicator_rect`]'s shape.
    pub fn app_indicator_rect(
        &self,
        i: usize,
        output_width: f64,
    ) -> Option<Rectangle<f64, Logical>> {
        if i >= self.app_indicators.len() {
            return None;
        }
        let rect = self.app_indicators_rect(output_width)?;
        let x = qs_slot_x(&icon_widths(self.app_indicators.len()), rect.loc.x, i) - QS_ICON_MARGIN;
        Some(Rectangle::new(
            Point::from((x, 0.)),
            Size::from((QS_ICON + 2. * QS_ICON_MARGIN, panel_height())),
        ))
    }

    /// Which indicator, if any, sits at an output-local position, and the rect it occupies — the
    /// id a click acts on, plus the anchor its menu hangs from. One walk, because a click needs
    /// both and looking the rect up again by id would be a second search for the same answer.
    pub fn app_indicator_hit(
        &self,
        pos: Point<f64, Logical>,
        output_width: f64,
    ) -> Option<(&str, Rectangle<f64, Logical>)> {
        (0..self.app_indicators.len()).find_map(|i| {
            self.app_indicator_rect(i, output_width)
                .filter(|rect| rect.contains(pos))
                .map(|rect| (self.app_indicators[i].id.as_str(), rect))
        })
    }

    /// Which indicator sits at an output-local position.
    pub fn app_indicator_at(&self, pos: Point<f64, Logical>, output_width: f64) -> Option<&str> {
        self.app_indicator_hit(pos, output_width).map(|(id, _)| id)
    }

    /// Adopt the indicators the StatusNotifier watcher currently has. Returns whether the panel
    /// changed, so the caller can queue a redraw.
    pub fn set_app_indicators(&mut self, indicators: Vec<PanelIndicator>) -> bool {
        if indicators == self.app_indicators {
            return false;
        }
        self.app_indicators = indicators;
        self.cache.borrow_mut().clear();
        true
    }

    /// The logical width a right-box role currently occupies, `0` when the role is absent
    /// (quickSettings is always present, the others come and go). The single source of
    /// truth for right-box presence, folded by [`Self::right_box_rect`] into placement.
    fn right_box_role_width(&self, role: &str) -> f64 {
        match role {
            ROLE_QUICK_SETTINGS => {
                qs_indicator_width(self.toggles, &self.system_status, self.audio, self.mic)
            }
            ROLE_APP_INDICATORS => self.app_indicators_width(),
            ROLE_SCREEN_RECORDING => self.recording_width(),
            ROLE_KEYBOARD => self.keyboard_width(),
            ROLE_A11Y => self.a11y_width(),
            ROLE_DATE_MENU => self.date_menu_width(),
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
                Rectangle::new(Point::from((right, 0.)), Size::from((0., panel_height())))
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
            let rect = Rectangle::new(
                Point::from((right - w, 0.)),
                Size::from((w, panel_height())),
            );
            if r == role {
                return Some(rect);
            }
            right -= w;
        }
        None
    }

    /// The panel's items with their current rectangles, for introspection and the
    /// (deferred) extension host. `output_width` is the output's logical width, used
    /// to place the right-anchored box (the status indicators and, past them, the clock).
    pub fn items(&self, output_width: f64, ws: WorkspaceState) -> Vec<PanelItem> {
        let mut items = vec![PanelItem {
            role: ROLE_ACTIVITIES,
            r#box: PanelBox::Left,
            rect: self.activities_rect(ws),
        }];
        // The right box, in `sessionMode.js:99` order plus our trailing dateMenu — each
        // role present only when it has a rect (screenRecording comes and goes with the
        // recording, like GNOME hiding the actor). A role's rect is its whole slot, so
        // the dateMenu's covers the messages dot as well as the clock button.
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

    /// The volume icon's own rect inside the quick-settings cluster, or `None` when there is no
    /// audio to show. GNOME puts the scroll handler on the volume indicator's actor, not on the
    /// whole status area (`js/ui/status/volume.js:434-437,470-472`), so this is what a
    /// scroll-to-change-volume must hit-test against.
    ///
    /// The box is the icon plus its `.system-status-icon` side margins, and the full panel height:
    /// an indicator is as tall as the bar it sits in.
    pub fn volume_indicator_rect(&self, output_width: f64) -> Option<Rectangle<f64, Logical>> {
        let audio = self.audio?;
        let want = crate::audio::volume_icon(&audio);
        let index = qs_indicator_icons(self.toggles, &self.system_status, self.audio, self.mic)
            .iter()
            .position(|(candidates, _)| candidates.first().is_some_and(|name| name == want))?;

        let rect = self.quick_settings_rect(output_width);
        let widths = qs_indicator_widths(self.toggles, &self.system_status, self.audio, self.mic);
        let x = qs_slot_x(&widths, rect.loc.x, index) - QS_ICON_MARGIN;
        Some(Rectangle::new(
            Point::from((x, 0.)),
            Size::from((widths[index] + 2. * QS_ICON_MARGIN, panel_height())),
        ))
    }

    /// Which panel *role*, if any, sits at an output-local logical position.
    /// `output_width` is needed to place the right-anchored box (the status indicators
    /// and, past them, the dateMenu).
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        output_width: f64,
        ws: WorkspaceState,
    ) -> Option<&'static str> {
        if self.activities_rect(ws).contains(pos) {
            Some(ROLE_ACTIVITIES)
        } else {
            // Every role's slot is its clickable extent, the dateMenu's included: its
            // slot is the whole `clock-display-box`, so the messages dot opens the
            // calendar like the clock does.
            RIGHT_BOX_ORDER.iter().copied().find(|&role| {
                self.right_box_rect(role, output_width)
                    .is_some_and(|rect| rect.contains(pos))
            })
        }
    }

    /// Render the bar for `output`. `overview_fade` is that monitor's overview
    /// progress (0 on the desktop, 1 with the overview fully open); it fades the
    /// panel background out, per [`bar_bg`].
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        ws: WorkspaceState,
        position: f64,
        overview_fade: f64,
        caches: DrawCaches<'_>,
    ) -> Vec<PanelElement> {
        let icons = caches.icons;
        let scale = output.current_scale().fractional_scale();
        let width = output_size(output).w;
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
        self.qs_indicator_elements(renderer, scale, width, &mut elements, icons, &mut cache);
        self.app_indicator_elements(renderer, scale, width, &mut elements, caches);

        // The screen-recording indicator's stop glyph, composited on top of its red pill
        // (which is drawn into the bar below). Same upload/caching as the QS cluster icons.
        if self.is_recording() {
            let r1 = self.screen_recording_rect(width);
            let icon_x = r1.loc.x + r1.size.w - INDICATOR_H_PADDING - R1_ICON;
            if let Some(tb) = SCREENCAST_STOP_ICONS
                .iter()
                .find_map(|n| icons.texture(renderer, n, R1_ICON, scale, TEXT))
            {
                let logical = tb.logical_size();
                let location = Point::from((icon_x, (panel_height() - logical.h) / 2.));
                elements.push(PanelElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        tb,
                        location,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
        }

        // The accessibility indicator's icon, centered in its button
        // (`accessibility.js:37-40`), on top of the shared `PILL_ROLES` container.
        if let Some(rect) = self.a11y_rect(width) {
            if let Some(tb) = A11Y_ICONS
                .iter()
                .find_map(|n| icons.texture(renderer, n, QS_ICON, scale, TEXT))
            {
                let logical = tb.logical_size();
                let location = Point::from((
                    rect.loc.x + (rect.size.w - logical.w) / 2.,
                    (panel_height() - logical.h) / 2.,
                ));
                elements.push(PanelElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        tb,
                        location,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
        }

        // The dateMenu messages-indicator dot (`message-indicator-symbolic`),
        // composited on top of the bar after the clock, in the panel fg.
        if let Some(rect) = self.messages_indicator_rect(width) {
            if let Some(tb) = icons.texture(
                renderer,
                "message-indicator-symbolic",
                MESSAGES_INDICATOR_ICON,
                scale,
                TEXT,
            ) {
                let logical = tb.logical_size();
                let location = Point::from((
                    rect.loc.x + (rect.size.w - logical.w) / 2.,
                    (panel_height() - logical.h) / 2.,
                ));
                elements.push(PanelElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        tb,
                        location,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
        }

        // The bar chrome (button containers, workspace dots, clock).
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
        // The keyboard input-source label. Its hover/checked pill is a shared
        // `PILL_ROLES` container drawn by `button_containers` above (the `InputSourceIndicator`
        // is a `PanelMenu.Button` like the clock); here we only place the label, left-aligned
        // at the button padding like the recording pill's label.
        let keyboard_label = self
            .keyboard_layout
            .as_ref()
            .zip(self.right_box_rect(ROLE_KEYBOARD, width))
            .map(|(label, kb)| (label.clone(), kb.loc.x + INDICATOR_H_PADDING));

        // Nothing animated may enter these bakes. Three things have been taken out of them,
        // each because it moved every frame and a bake is a GPU round trip: the overview
        // fade is the bar *background*'s alpha, its own element below; the button-container
        // fills are the pills' alpha, their own elements too ([`pill_element`]); and the
        // workspace dots, whose geometry morphs through a switch, are rounded-solid
        // elements ([`workspace_dots`]). Before the first two, opening the overview checked
        // the Activities button and the bar re-baked on every frame of the fade; before the
        // third, every workspace switch did the same.
        //
        // The labels are the fourth, taken out for the *slower* version of the same reason.
        // They used to share one bake the full width of the output, so a label changing
        // re-rasterized the whole bar: 0.9 ms warm, but at a once-a-minute cadence the path is
        // never warm, and a live seat measured p50 60.7 ms / p99 311 ms for it — three or four
        // dropped vblanks every single minute, forever, to change a digit. Each label is its
        // own label-sized texture now, keyed on its own text, so a clock tick re-bakes the
        // clock. `position` is deliberately not consulted here.
        // `the_overview_animation_rebakes_nothing_per_frame` and
        // `the_workspace_switch_rebakes_nothing_per_frame` are the guards — both were written
        // against the old single bake. They only see *bakes*, though: a cached texture rewrapped
        // in a fresh buffer every frame bakes nothing and still churns the element identity,
        // which is what `nothing_churns_its_element_id_per_frame` covers — hence the cache holds
        // the `TextureBuffer`, not the texture.
        //
        // The recording label is why all three moved rather than just the clock: it is an `M:SS`
        // elapsed timer, so an active recording re-baked the full bar *every second*. Splitting
        // the clock alone would have left the same bug with a rarer trigger.
        let labels = [(
            LabelSlot::Clock,
            self.clock_text.as_str(),
            self.date_menu_rect(width).loc.x + clock_h_padding(),
        )]
        .into_iter()
        .chain(
            recording_label
                .as_ref()
                .map(|(s, x)| (LabelSlot::Recording, s.as_str(), *x)),
        )
        .chain(
            keyboard_label
                .as_ref()
                .map(|(s, x)| (LabelSlot::Keyboard, s.as_str(), *x)),
        );
        for (slot, text, x) in labels {
            match label_element(renderer, &mut cache, scale, scale_key, slot, text, x) {
                Ok(element) => elements.push(element),
                Err(err) => {
                    tracing::error!("error drawing the panel {slot:?} label: {err:#}");
                    return elements;
                }
            }
        }

        // The workspace dots, over the pills and the background but under nothing they
        // overlap (the clock and labels in the chrome sit elsewhere in the bar). One
        // element each, drawn straight into the frame — see [`workspace_dots`].
        let dots = workspace_dots(ws.count, position);
        cache.dots.resize_with(dots.len(), RoundedSolidBuffer::new);
        for (buffer, (rect, radius, color)) in cache.dots.iter_mut().zip(&dots) {
            buffer.update(rect.size, *radius, *color);
            elements.push(PanelElement::RoundedSolid(
                RoundedSolidRenderElement::from_buffer(
                    buffer,
                    rect.loc,
                    Scale::from(scale),
                    Kind::Unspecified,
                ),
            ));
        }

        // The button-container pills, after the chrome so they composite *under* it
        // (this list is front-to-back) and before the background so they sit over it —
        // the same place they occupied when they were painted at the bottom of the
        // chrome bake. Each is a cached stadium drawn at its current fade alpha.
        for (rect, color) in &containers {
            match pill_element(renderer, &mut cache.pills, scale, *rect, *color) {
                Ok(element) => elements.push(element),
                Err(err) => tracing::error!("error drawing a panel button container: {err:#}"),
            }
        }

        // The bar background wash, under everything pushed above (this list is front-to-back).
        // Its alpha is the only thing the overview animates, which is exactly why it is not baked
        // into the chrome.
        let bg = bar_bg(overview_fade);
        cache
            .bg
            .update(Size::from((width, panel_height())), Color32F::from(bg));
        if bg[3] > 0. {
            elements.push(PanelElement::Solid(SolidColorRenderElement::from_buffer(
                &cache.bg,
                Point::from((0., 0.)),
                1.,
                Kind::Unspecified,
            )));
        }

        // Bottom of the bar: the blurred capture of the scene behind it — see [`BAR_BG`]. It stays
        // in through the overview: the backdrop revealed there is itself a blurred wallpaper
        // (`Synoik::render_inner`), so a bar that kept a hard edge on it would read as a band, and
        // the wash above fading to nothing is what makes the two meet.
        //
        // `noise`/`saturation` are left neutral: they are the surface-effect path's decoration, and
        // a neutral pass has the property that blurring a flat colour returns that same colour —
        // which is what keeps a wallpaper-less session (and the render corpus) showing the panel
        // over exactly the fill that is behind it.
        let geometry = Rectangle::new(Point::from((0., 0.)), Size::from((width, panel_height())));
        elements.push(PanelElement::Backdrop(self.backdrop.render(
            None,
            RenderParams {
                geometry,
                subregion: None,
                clip: None,
                scale,
            },
            Some(BAR_BLUR.into()),
            Finish::NONE,
        )));

        elements
    }

    /// Push the right-box quick-settings status icons onto `elements`, laid out in
    /// a right-anchored cluster and composited on top of the bar. Each icon is
    /// resolved from its candidate list; the upload itself is the [`IconCache`]'s.
    /// The app-indicator cluster's icons, laid out exactly as the quick-settings cluster's are.
    ///
    /// An indicator whose icon does not resolve still keeps its slot: the alternative is icons
    /// that shuffle sideways whenever a client changes to a name the theme lacks, and the slot is
    /// what a click is aimed at.
    fn app_indicator_elements(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        output_width: f64,
        elements: &mut Vec<PanelElement>,
        caches: DrawCaches<'_>,
    ) {
        let DrawCaches { icons, images } = caches;
        let Some(rect) = self.app_indicators_rect(output_width) else {
            return;
        };

        for (i, indicator) in self.app_indicators.iter().enumerate() {
            // Three forms, one slot. A themed name is tinted like every other status icon; a file
            // or a pixmap keeps the client's own colours, because it *is* the client's artwork —
            // recoloring an app's logo to the panel foreground would be inventing a look for it.
            let tb = match &indicator.icon {
                ItemIcon::None => None,
                ItemIcon::Themed(name) => icons.texture(renderer, name, QS_ICON, scale, TEXT),
                ItemIcon::File(path) => {
                    // `Contain` is what makes a non-square icon safe: `indicator-multiload` sends
                    // a wide strip and expects its aspect kept, and letterboxing it inside the
                    // slot is exactly that, with no special case.
                    icons.texture_for_buffer(
                        renderer,
                        &path.display().to_string(),
                        QS_ICON,
                        scale,
                        || {
                            let source = crate::image_source::ImageSource::File(path.clone());
                            images.buffer(&source, ImageFit::Contain, QS_ICON, scale)
                        },
                    )
                }
                ItemIcon::Pixmap(pixmap) => icons.texture_for_buffer(
                    renderer,
                    &format!("pixmap:{:016x}", pixmap.hash),
                    QS_ICON,
                    scale,
                    || {
                        crate::render_helpers::icon::buffer_from_premultiplied_rgba(
                            &pixmap.rgba,
                            pixmap.width,
                            pixmap.height,
                            QS_ICON,
                            scale,
                        )
                    },
                ),
            };
            let Some(tb) = tb else {
                continue;
            };

            // A pixmap arrives at whatever size the client chose, so it is centred in its slot at
            // its natural size rather than stretched to the icon box.
            let logical = tb.logical_size();
            let location = Point::from((
                qs_slot_x(&icon_widths(self.app_indicators.len()), rect.loc.x, i)
                    + (QS_ICON - logical.w) / 2.,
                (panel_height() - logical.h) / 2.,
            ));
            elements.push(PanelElement::Texture(
                TextureRenderElement::from_texture_buffer(
                    tb,
                    location,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ),
            ));
        }
    }

    fn qs_indicator_elements(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        output_width: f64,
        elements: &mut Vec<PanelElement>,
        icons: &IconCache,
        cache: &mut BarCache,
    ) {
        let rect = self.quick_settings_rect(output_width);
        let slots = qs_indicator_slots(self.toggles, &self.system_status, self.audio, self.mic);
        // Slot positions come from the shared width walk, never from a running cursor: a slot
        // whose icon fails to resolve must still consume its place, or every icon after it slides
        // left of where the hit tests look for it.
        let widths: Vec<f64> = slots.iter().map(QsSlot::width).collect();
        for (i, slot) in slots.iter().enumerate() {
            // The first icon carries the box's leading `.system-status-icon` left margin.
            let x = qs_slot_x(&widths, rect.loc.x, i);
            match slot {
                QsSlot::Icon(candidates, color) => {
                    // The first candidate that resolves, in its tint.
                    let Some(tb) = candidates
                        .iter()
                        .find_map(|name| icons.texture(renderer, name, QS_ICON, scale, *color))
                    else {
                        continue;
                    };
                    let logical = tb.logical_size();
                    let location = Point::from((x, (panel_height() - logical.h) / 2.));
                    elements.push(PanelElement::Texture(
                        TextureRenderElement::from_texture_buffer(
                            tb,
                            location,
                            1.,
                            None,
                            None,
                            Kind::Unspecified,
                        ),
                    ));
                }
                QsSlot::Battery(battery) => {
                    self.battery_elements(renderer, scale, x, battery, elements, icons, cache);
                }
            }
        }
    }

    /// The dynamic battery indicator at slot x: its baked body, plus the charging bolt composited
    /// over it from the icon path like every other glyph in the cluster
    /// (`docs/fork/battery-indicator-design.md`).
    #[allow(clippy::too_many_arguments)]
    fn battery_elements(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        x: f64,
        battery: &system_status::BatteryStatus,
        elements: &mut Vec<PanelElement>,
        icons: &IconCache,
        cache: &mut BarCache,
    ) {
        let Ok(scale_key) = NotNan::new(scale) else {
            return;
        };
        let look = system_status::battery_look(battery);
        let fill = battery.percentage / 100.;
        let key = (scale_key, battery_revision(&look, fill));
        let y = (panel_height() - widget::Battery::HEIGHT) / 2.;

        // Re-bake only when the drawn shape actually changes. The percentage moves a few times an
        // hour, so this cache holds one entry that survives essentially the whole session.
        if cache.battery.as_ref().map(|c| c.key) != Some(key) {
            let w = widget::Battery {
                fill,
                body_tint: widget::battery_tint(look.body),
                fill_tint: widget::battery_tint(look.fill),
            };
            let size = Size::from((widget::Battery::WIDTH, widget::Battery::HEIGHT));
            let baked = widget::bake_uncached(renderer, scale, size, |frame, phys| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(widget::style::TRANSPARENT)?;
                p.battery(Point::from((0., 0.)), &w)
            });
            match baked {
                Ok(texture) => {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    cache.battery = Some(CachedBattery { key, buffer });
                }
                Err(err) => {
                    tracing::error!("error baking the battery indicator: {err:#}");
                    return;
                }
            }
        }

        // The glyph sits over the body, so it is pushed first: elements are consumed in reverse.
        if let Some(g) = widget::battery_overlay_glyph(look.overlay) {
            // Centred on the *body*, not on the slot: the nub is not part of the battery's face.
            let center = Point::from((x + widget::Battery::BODY_W / 2., panel_height() / 2.));
            elements.extend(
                widget::battery_overlay_elements(
                    renderer,
                    icons,
                    &g,
                    scale,
                    Point::from((0., 0.)),
                    center,
                )
                .into_iter()
                .map(PanelElement::Texture),
            );
        }

        if let Some(cached) = &cache.battery {
            elements.push(PanelElement::Texture(
                TextureRenderElement::from_texture_buffer(
                    cached.buffer.clone(),
                    Point::from((x, y)),
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ),
            ));
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
pub(crate) fn strftime_format(fmt: ClockFormat) -> &'static str {
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
    strftime_local(now, strftime_format(fmt))
}

/// Format epoch seconds through the C library's locale-aware `strftime`.
///
/// Shared with the lock screen, whose date line is a `strftime` string GNOME passes through
/// `Shell.util_translate_time_string` (`unlockDialog.js:411-415`) — same mechanism, different
/// format, so the unsafe block lives in one place.
pub(crate) fn strftime_local(now: libc::time_t, format: &str) -> String {
    // SAFETY: localtime returns a pointer into a static buffer; we pass it
    // straight to strftime before any other libc time call touches it.
    unsafe {
        let tm = libc::localtime(&now);
        if tm.is_null() {
            return String::new();
        }
        let Ok(format) = std::ffi::CString::new(format) else {
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

/// The workspace dots' geometry: one rounded rect per workspace, continuously expanded
/// around the active one so the row morphs smoothly while switching.
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
/// left, vertically centered; fully rounded (`$forced_circular_radius`).
///
/// Pure geometry, in panel-local logical coordinates: every returned dot becomes a
/// [`RoundedSolidRenderElement`] drawn straight into the frame. It is emphatically *not*
/// baked. All three of a dot's properties — width, height and opacity — move on every
/// frame of a switch, so a bake would miss its cache every frame and cost a GPU round
/// trip each time. That was this widget's cost until 2026-07-26, and it is the last
/// per-frame bake the guardrail tests knew about.
fn workspace_dots(count: usize, position: f64) -> Vec<(Rectangle<f64, Logical>, f64, [f32; 4])> {
    // Clamp into range so an out-of-range position — a spring overshoot, or a stale
    // active index — parks the wide pill on the end dot instead of expanding none of them.
    let position = position.clamp(0., count.saturating_sub(1) as f64);
    let mult = width_multiplier(count);
    let band_cy = panel_height() / 2.;

    let mut dots = Vec::with_capacity(count);
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
            Point::<f64, Logical>::from((slot_cx - draw_w / 2., band_cy - draw_h / 2.)),
            Size::<f64, Logical>::from((draw_w, draw_h)),
        );
        // Half the height clamps to a full circle (small dot) or stadium (pill).
        dots.push((rect, draw_h / 2., [1., 1., 1., opacity]));
        x += slot_w + DOT_SPACING;
    }
    dots
}

/// Draw the bar chrome into an offscreen [`VkTexture`]: the clock glyph run (at `clock_x`,
/// its button's padded left edge) and the recording/keyboard labels, over a
/// **transparent** background. The hover/active
/// button containers and the workspace dots are not here — they animate, and each is its
/// own element. The returned texture is `SHADER_READ_ONLY`
/// (sampleable) so the caller can composite it directly. The right-box status icons
/// are composited separately, on top, and the bar background is a separate solid
/// element underneath.
///
/// Keeping the background out is what makes this bake cacheable across an overview
/// animation. The background alpha changes every frame while the overview opens
/// ([`bar_bg`]); the chrome does not, and a bake is a GPU round trip — the single
/// most expensive thing a frame can do on this stack.
/// The cached stadium behind [`pill_element`]: `size` filled with `color`'s RGB at
/// **full alpha**, whatever alpha `color` carries. The fade lives on the element.
fn pill_texture(
    renderer: &mut VulkanRenderer,
    cache: &mut widget::BakeCache,
    scale: f64,
    size: Size<f64, Logical>,
    color: [f32; 4],
) -> anyhow::Result<TextureBuffer<VkTexture>> {
    let opaque = [color[0], color[1], color[2], 1.];
    // `widget::bake` keys on (scale, physical size), so the colour has to ride in the
    // revision or two pills of the same shape in different colours would alias. Alpha is
    // deliberately absent: the bake is opaque and the fade rides the element
    // (`pill_element`), which is what keeps a container fade from re-baking per frame.
    let revision = widget::Revision::new().color(opaque).done();
    // The bake buffer *is* the pill, so its local box sits at the origin.
    let local = Rectangle::new(Point::from((0., 0.)), size);
    widget::bake(
        renderer,
        cache,
        scale,
        size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear([0., 0., 0., 0.])?;
            // Half the height clamps the SDF to a stadium (fully rounded pill).
            p.fill_rounded(local, local.size.h / 2., opaque)?;
            Ok(())
        },
    )
}

/// Bake (once per shape) one button-container pill — a stadium in `color`'s RGB at
/// full alpha — and return it as an element placed at `rect`, composited at
/// `color`'s alpha.
///
/// The alpha rides on the element rather than the texture on purpose: it is the only
/// thing that animates (a hover fade, the Activities button's checked fade when the
/// overview opens), and a composite-time alpha costs nothing while an alpha baked
/// into the texture costs a full GPU round trip on every frame of the fade. That was
/// the panel's per-frame bake through the whole overview animation.
///
/// The colour is in the bake revision, not the alpha: `widget::bake` keys on
/// `(scale, size)`, and the two colours in use (the white hover/checked fill and the
/// screen-recording pill's red) are constant.
fn pill_element(
    renderer: &mut VulkanRenderer,
    cache: &mut widget::BakeCache,
    scale: f64,
    rect: Rectangle<f64, Logical>,
    color: [f32; 4],
) -> anyhow::Result<PanelElement> {
    let buffer = pill_texture(renderer, cache, scale, rect.size, color)?;
    Ok(PanelElement::Texture(
        TextureRenderElement::from_texture_buffer(
            buffer,
            rect.loc,
            color[3],
            None,
            None,
            Kind::Unspecified,
        ),
    ))
}

/// One bar label as a composited element, baked on miss and cached on its own text.
///
/// `x` is the label's padded left edge in logical coordinates — the same anchor the old
/// full-width bake used as the run's origin — and the element is placed so the glyph ink
/// lands on the exact physical column it did then.
fn label_element(
    renderer: &mut VulkanRenderer,
    cache: &mut BarCache,
    scale: f64,
    scale_key: NotNan<f64>,
    slot: LabelSlot,
    text: &str,
    x: f64,
) -> anyhow::Result<PanelElement> {
    let key = (scale_key, slot);
    // The bake is keyed on the label's own text, so the clock ticking leaves the recording
    // and keyboard labels alone — and vice versa. Comparing rather than keying by `String`
    // keeps one entry per slot instead of one per distinct value ever shown: a clock keyed
    // by its text would grow an entry a minute.
    let hit = cache
        .textures
        .get(&key)
        .is_some_and(|cached| cached.text == text);
    if !hit {
        let (texture, lead_px) = draw_label_texture(renderer, scale, text)?;
        let buffer = TextureBuffer::from_texture(
            renderer,
            texture,
            scale,
            Transform::Normal,
            // Transparent everywhere it does not draw, so it never occludes. Nothing in the
            // panel does any more: the background below it is a translucent wash over a
            // blurred backdrop, so the strip has no opaque region at all and whatever is
            // under the bar still has to be drawn.
            Vec::new(),
        );
        cache.textures.insert(
            key,
            CachedLabel {
                text: text.to_owned(),
                buffer,
                lead_px,
            },
        );
    }
    let cached = cache.textures.get(&key).expect("just inserted");
    // Place by the *physical* column the run used to be drawn at, converted back to logical:
    // feeding the raw logical `x` through the element would round a second time, a hair away
    // from where every other panel measurement (all of which are physical-rounded) puts it.
    let loc = Point::<f64, Logical>::from((
        f64::from(to_physical_precise_round::<i32>(scale, x) + cached.lead_px) / scale,
        0.,
    ));
    Ok(PanelElement::Texture(
        TextureRenderElement::from_texture_buffer(
            cached.buffer.clone(),
            loc,
            1.,
            None,
            None,
            Kind::Unspecified,
        ),
    ))
}

/// Bake one bar label into a label-sized texture. Returns it with the physical-px offset from
/// the label's anchor column to the texture's left edge (`<= 0`, non-zero only for a glyph whose
/// ink starts left of its origin), which [`label_element`] adds back when it places the element.
fn draw_label_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    text: &str,
) -> anyhow::Result<(VkTexture, i32)> {
    let _span = tracy_client::span!("panel::draw_label_texture");

    let height_px: i32 = to_physical_precise_round(scale, panel_height());
    let height_px = height_px.max(1);

    // Shape up front (needs `&mut renderer`, before the bake frame opens). `TextShaper` owns the
    // pt → physical-px multiply; bar labels draw bold, like GNOME's `panel_button`.
    let run = {
        let mut shaper = TextShaper::new(renderer, scale);
        shaper.shape(text, TextStyle::new(FONT_PT).bold())?
    };

    // A label sits at its button's own padding, and the button's width came from the *advance*
    // measurement (`clock_button_width`), not the ink. gnome-shell's WallClock uses tabular
    // figures, so the advance width is constant as the seconds tick and the label never shifts;
    // sizing on the ink (whose left edge/width wobble per digit) would make the run jitter
    // left/right each second. Our Cantarell digits are tabular too, so an advance-derived origin
    // is rock-steady — which is why the *texture* may be ink-sized without the glyphs moving:
    // the run keeps its ink offset from the anchor, the texture is just cropped to it.
    let (ink_x, _, ink_w, _) = run.ink_bounds();
    let lead_px = ink_x.min(0);
    let width_px = (ink_x + ink_w - lead_px).max(1);
    // Vertical centers on the font line-box (ascent+descent about the baseline), as St/Pango do —
    // reserving descent space so the caps sit a hair higher than ink centering would put them
    // (GNOME's clock reads visually higher in the bar). The band is the full panel height, as it
    // was when every label shared one bar-tall texture, so nothing moves vertically either.
    let origin = Point::<i32, Physical>::from((-lead_px, run.line_box_centered_y(height_px)));

    let size = Size::<i32, Physical>::from((width_px, height_px));
    let texture = widget::bake_uncached_sized(renderer, size, |frame| {
        let mut p = Painter::new(frame, scale, size);

        p.clear([0., 0., 0., 0.])?;
        // The button containers are NOT here: they are their own elements, composited under
        // this texture ([`pill_element`]). Their only animated property is alpha, and alpha is
        // free at composite time — in here it made the whole bar uncacheable for the length of
        // every hover and every overview open. Neither are the workspace dots
        // ([`workspace_dots`]), for the stronger version of the same reason: their geometry
        // animates, and geometry cannot ride an element's alpha.
        p.text_px(&run, origin, TEXT)?;
        Ok(())
    })?;
    Ok((texture, lead_px))
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

    /// The volume icon has its OWN reactive box inside the status cluster: gnome-shell puts the
    /// scroll handler on that indicator's actor (`js/ui/status/volume.js:434-437,470-472`), not on
    /// the cluster. Its neighbours must fall outside it, or a scroll aimed at the network icon
    /// would change the volume.
    #[test]
    fn the_volume_icon_has_its_own_hit_box() {
        use crate::system_status::{BatteryStatus, NetworkStatus, SystemStatus};

        let mut panel = test_panel();
        let output_width = 1920.;
        // No audio at all -> nothing to scroll on.
        assert!(panel.volume_indicator_rect(output_width).is_none());

        panel.set_system_status(SystemStatus {
            network: NetworkStatus::Wired,
            battery: Some(BatteryStatus {
                icon_name: "battery-level-90-symbolic".to_string(),
                percentage: 90.,
                ..Default::default()
            }),
            ..Default::default()
        });
        panel.set_audio(Some(AudioStatus {
            volume: 0.5,
            muted: false,
        }));

        // The cluster is network, volume, battery: the volume box must sit strictly between the
        // other two icons, and inside the cluster.
        let cluster = panel.quick_settings_rect(output_width);
        let rect = panel.volume_indicator_rect(output_width).unwrap();
        assert!(
            cluster.loc.x <= rect.loc.x
                && rect.loc.x + rect.size.w <= cluster.loc.x + cluster.size.w,
            "the volume box must be inside the cluster: {rect:?} vs {cluster:?}"
        );
        assert_eq!(
            rect.size.h,
            panel_height(),
            "an indicator is as tall as the bar"
        );

        // The icon centres either side of it -- the neighbours -- are NOT in the box.
        let widths = icon_widths(3);
        let centre =
            |i: usize| Point::from((qs_slot_x(&widths, cluster.loc.x, i) + widths[i] / 2., 10.));
        assert!(
            rect.contains(centre(1)),
            "the volume icon is the second of three"
        );
        assert!(
            !rect.contains(centre(0)),
            "the network icon must not scroll volume"
        );
        assert!(
            !rect.contains(centre(2)),
            "the battery icon must not scroll volume"
        );

        // It tracks the icon set: muting swaps the glyph but keeps the same slot, while dropping
        // the battery leaves the volume icon last and moves its box.
        panel.set_audio(Some(AudioStatus {
            volume: 0.,
            muted: true,
        }));
        assert_eq!(panel.volume_indicator_rect(output_width), Some(rect));
        panel.set_system_status(SystemStatus {
            network: NetworkStatus::Wired,
            ..Default::default()
        });
        let moved = panel.volume_indicator_rect(output_width).unwrap();
        assert_ne!(moved, rect, "a shorter cluster re-places the volume icon");
    }

    /// A cluster slot may be wider than `QS_ICON` — that is the whole point of walking widths
    /// instead of `i * QS_ICON`, and what the dynamic battery indicator will need
    /// (`docs/fork/battery-indicator-design.md`). Every slot is an icon today, so this pins the
    /// geometry primitive directly: a wide slot must push its successors right by exactly its
    /// excess, and the cluster must grow by the same amount.
    #[test]
    fn a_wide_cluster_slot_displaces_the_slots_after_it() {
        let wide = QS_ICON + 13.5;
        let uniform = icon_widths(3);
        let mixed = vec![QS_ICON, wide, QS_ICON];

        // Slots before the wide one do not move; slots after it shift by the excess.
        assert_eq!(qs_slot_x(&mixed, 0., 0), qs_slot_x(&uniform, 0., 0));
        assert_eq!(qs_slot_x(&mixed, 0., 1), qs_slot_x(&uniform, 0., 1));
        assert_eq!(
            qs_slot_x(&mixed, 0., 2),
            qs_slot_x(&uniform, 0., 2) + (wide - QS_ICON),
            "the slot after a wide one moves right by exactly its excess"
        );

        // The cluster grows by the excess, and by nothing else: the gaps are unchanged.
        assert_eq!(
            qs_cluster_width(&mixed),
            qs_cluster_width(&uniform) + (wide - QS_ICON)
        );

        // A single-slot cluster carries no gap, and a wide lone slot is still measured.
        assert_eq!(
            qs_cluster_width(&[wide]) - qs_cluster_width(&icon_widths(1)),
            wide - QS_ICON
        );
    }

    /// The battery is the one cluster slot that is not `QS_ICON` wide, so the cluster must
    /// reserve its real width and every hit test must survive it. This is the regression the
    /// per-element-width walk exists to prevent: with fixed slots the battery drew over its
    /// neighbour and a volume scroll landed on the wrong icon.
    #[test]
    fn the_battery_slot_reserves_its_own_width_and_shifts_nothing_onto_it() {
        use crate::system_status::{BatteryStatus, NetworkStatus, SystemStatus};

        let output_width = 1920.;
        let mut panel = test_panel();
        panel.set_system_status(SystemStatus {
            network: NetworkStatus::Wired,
            battery: Some(BatteryStatus {
                icon_name: "battery-level-90-symbolic".to_string(),
                percentage: 90.,
                ..Default::default()
            }),
            ..Default::default()
        });
        panel.set_audio(Some(AudioStatus {
            volume: 0.5,
            muted: false,
        }));

        // Cluster order is network, volume, battery — and the battery slot is the wide one.
        let widths =
            qs_indicator_widths(panel.toggles, &panel.system_status, panel.audio, panel.mic);
        assert_eq!(widths, vec![QS_ICON, QS_ICON, widget::Battery::WIDTH]);

        // The cluster is wider than three icons by exactly the battery's excess.
        let cluster = panel.quick_settings_rect(output_width);
        assert_eq!(
            cluster.size.w,
            qs_cluster_width(&icon_widths(3)) + (widget::Battery::WIDTH - QS_ICON),
            "the cluster must reserve the battery's real width"
        );

        // The volume box sits between them and touches neither neighbour's centre.
        let volume = panel.volume_indicator_rect(output_width).unwrap();
        let centre =
            |i: usize| Point::from((qs_slot_x(&widths, cluster.loc.x, i) + widths[i] / 2., 10.));
        assert!(volume.contains(centre(1)), "the volume icon is the second");
        assert!(
            !volume.contains(centre(0)),
            "the network icon must not scroll volume"
        );
        assert!(
            !volume.contains(centre(2)),
            "the battery must not scroll volume"
        );

        // And the battery's whole slot clears the volume box, not merely its centre: a wide slot
        // whose width was ignored would start inside its neighbour.
        let battery_x = qs_slot_x(&widths, cluster.loc.x, 2);
        assert!(
            battery_x >= volume.loc.x + volume.size.w,
            "the battery slot starts at or after the volume box ends"
        );
        assert!(
            battery_x + widget::Battery::WIDTH <= cluster.loc.x + cluster.size.w,
            "and ends inside the cluster"
        );
    }

    fn indicator(id: &str, icon: Option<&str>) -> PanelIndicator {
        PanelIndicator {
            id: id.to_owned(),
            icon: match icon {
                Some(name) => ItemIcon::Themed(name.to_owned()),
                None => ItemIcon::None,
            },
        }
    }

    /// With no indicators the cluster takes no room at all — an empty slot would push every other
    /// right-box role leftward for nothing.
    #[test]
    fn an_empty_app_indicator_cluster_occupies_nothing() {
        let output_width = 1920.;
        let mut panel = test_panel();
        assert_eq!(panel.app_indicators_rect(output_width), None);

        let before = panel.quick_settings_rect(output_width);
        assert!(panel.set_app_indicators(vec![indicator("a", Some("foo"))]));
        assert!(panel.app_indicators_rect(output_width).is_some());
        assert_eq!(
            panel.quick_settings_rect(output_width),
            before,
            "app indicators lead the right box, so nothing to their right may move"
        );
    }

    /// Each indicator gets its own hit box inside the shared slot, and they do not overlap —
    /// the cluster is one panel *role* but several click targets, like the QS status icons.
    #[test]
    fn each_app_indicator_has_its_own_hit_box() {
        let output_width = 1920.;
        let mut panel = test_panel();
        panel.set_app_indicators(vec![
            indicator("first", Some("a")),
            indicator("second", Some("b")),
            indicator("third", None),
        ]);

        let rects: Vec<_> = (0..3)
            .map(|i| panel.app_indicator_rect(i, output_width).unwrap())
            .collect();
        assert_eq!(panel.app_indicator_rect(3, output_width), None);

        for pair in rects.windows(2) {
            assert!(
                pair[0].loc.x + pair[0].size.w <= pair[1].loc.x,
                "indicator boxes must not overlap: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }

        // The whole cluster's slot contains all three, and a click in each lands on its own id.
        let cluster = panel.app_indicators_rect(output_width).unwrap();
        for (rect, id) in rects.iter().zip(["first", "second", "third"]) {
            let center = Point::from((rect.loc.x + rect.size.w / 2., panel_height() / 2.));
            assert!(cluster.contains(center));
            assert_eq!(panel.app_indicator_at(center, output_width), Some(id));
            assert_eq!(
                panel.hit_test(
                    center,
                    output_width,
                    WorkspaceState {
                        count: 2,
                        active: 0
                    }
                ),
                Some(ROLE_APP_INDICATORS)
            );
        }

        // An icon that failed to resolve still holds its slot, or its neighbours would slide
        // sideways whenever a client picked a name the theme lacks.
        assert!(rects[2].size.w > 0.);
    }

    /// The cluster grows leftward as indicators arrive: the roles to its right keep their place.
    #[test]
    fn the_app_indicator_cluster_grows_leftward() {
        let output_width = 1920.;
        let mut panel = test_panel();
        panel.set_app_indicators(vec![indicator("a", Some("a"))]);
        let one = panel.app_indicators_rect(output_width).unwrap();
        let qs = panel.quick_settings_rect(output_width);

        panel.set_app_indicators(vec![indicator("a", Some("a")), indicator("b", Some("b"))]);
        let two = panel.app_indicators_rect(output_width).unwrap();

        assert!(two.size.w > one.size.w);
        assert!(two.loc.x < one.loc.x, "the cluster extends to the left");
        assert_eq!(
            two.loc.x + two.size.w,
            one.loc.x + one.size.w,
            "its right edge is anchored"
        );
        assert_eq!(panel.quick_settings_rect(output_width), qs);
    }

    /// Setting the same list twice is not a change, so a client repainting an unchanged icon
    /// cannot drive a redraw per property signal.
    #[test]
    fn re_setting_identical_indicators_is_not_a_change() {
        let mut panel = test_panel();
        let list = vec![indicator("a", Some("foo"))];
        assert!(panel.set_app_indicators(list.clone()));
        assert!(!panel.set_app_indicators(list));
        assert!(panel.set_app_indicators(vec![indicator("a", Some("bar"))]));
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
                ..Default::default()
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

    /// The bluetooth indicator is visible iff any device is connected (`bluetooth.js:458-464`)
    /// and sits between the network slot and the airplane icon (GNOME's `_indicators` order:
    /// network … bluetooth, rfkill — `panel.js:351-357`).
    #[test]
    fn bluetooth_indicator_shows_only_with_a_connected_device() {
        use crate::system_status::{
            AirplaneStatus, BluetoothDevice, BluetoothStatus, BtAdapterState, NetworkStatus,
            SystemStatus,
        };

        let toggles = QuickToggles::default();
        let no_mic = MicStatus::default();
        let device = |connected| BluetoothDevice {
            path: "/org/bluez/hci0/dev_AA".to_string(),
            alias: "Buds".to_string(),
            icon: None,
            connectable: true,
            paired: true,
            trusted: false,
            connected,
        };
        let bt = |connected| BluetoothStatus {
            adapter: Some("/org/bluez/hci0".to_string()),
            adapter_present: true,
            powered: true,
            state: BtAdapterState::On,
            devices: vec![device(connected)],
        };

        // Powered adapter, nothing connected → no icon.
        let idle = SystemStatus {
            network: NetworkStatus::Wired,
            bluetooth: bt(false),
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &idle, None, no_mic);
        assert!(!icons.iter().any(|c| c.0[0].starts_with("bluetooth")));

        // A connected device → the icon, after network, before airplane.
        let connected = SystemStatus {
            network: NetworkStatus::Wired,
            bluetooth: bt(true),
            airplane: AirplaneStatus {
                active: true,
                show: true,
            },
            ..Default::default()
        };
        let icons = qs_indicator_icons(toggles, &connected, None, no_mic);
        let names: Vec<_> = icons.iter().map(|c| c.0[0].as_str()).collect();
        assert_eq!(
            names,
            [
                "network-wired-symbolic",
                "bluetooth-active-symbolic",
                "airplane-mode-symbolic"
            ]
        );
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
                ..Default::default()
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

    /// The dateMenu messages-indicator dot (`js/ui/dateMenu.js:871-886`): the setter is a
    /// no-op-detecting toggle, and the dot draws inside the clock button's own trailing
    /// padding.
    ///
    /// **The invariant is that the dot costs no layout.** GNOME can afford a dot that
    /// occupies width because its clock is centred and a mirrored pad absorbs it; ours is
    /// right-anchored, so any width the dot took would be width the clock — and every
    /// status indicator left of it — gave back the moment a notification arrived or was
    /// read. Asserted as strict rect equality across the toggle, not a tolerance.
    /// Structural, no GPU.
    #[test]
    fn the_messages_dot_moves_nothing() {
        let ow = 1920.;
        let mut panel = test_panel();

        // Hidden by default: no dot rect, and the button owns the output's right corner.
        assert!(!panel.messages_indicator_visible());
        assert!(panel.messages_indicator_rect(ow).is_none());
        let clock = panel.date_menu_rect(ow);
        let qs = panel.quick_settings_rect(ow);
        assert_eq!(
            clock.loc.x + clock.size.w,
            ow,
            "the clock button owns the output's right corner"
        );

        // Show it: the setter reports the change once, then no-ops.
        assert!(panel.set_messages_indicator(true));
        assert!(!panel.set_messages_indicator(true), "no-op re-set");
        assert!(panel.messages_indicator_visible());

        // Nothing moved — not the button, not the cluster left of it.
        assert_eq!(
            panel.date_menu_rect(ow),
            clock,
            "the clock button must not move"
        );
        assert_eq!(
            panel.quick_settings_rect(ow),
            qs,
            "the status cluster must not move either"
        );

        // The dot is a 16px square in the button's trailing padding, held clear of both
        // things it could collide with: the clock label and the pill's rounded end.
        let dot = panel.messages_indicator_rect(ow).unwrap();
        assert_eq!(dot.size.w, MESSAGES_INDICATOR_ICON);
        let label_right = clock.loc.x + clock.size.w - clock_h_padding();
        let pill_right = container_rect(clock).loc.x + container_rect(clock).size.w;
        assert!(
            dot.loc.x >= label_right,
            "the dot must not overlap the clock label ({} vs {label_right})",
            dot.loc.x,
        );
        assert_eq!(
            pill_right - (dot.loc.x + dot.size.w),
            MESSAGES_INDICATOR_GAP,
            "the dot must stop short of the pill's rounded end, not sit flush in it"
        );

        // And it is the same button, so clicking the dot opens the calendar.
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
    ///
    /// Our divergence lives here: the dateMenu is in the RIGHT box, past quickSettings,
    /// not alone in GNOME's centre box ([`RIGHT_BOX_ORDER`]).
    #[test]
    fn items_expose_roles_and_boxes() {
        let panel = test_panel();
        let ow = 1920.;
        let items = panel.items(
            ow,
            WorkspaceState {
                count: 3,
                active: 1,
            },
        );
        let activities = items.iter().find(|i| i.role == ROLE_ACTIVITIES).unwrap();
        let date = items.iter().find(|i| i.role == ROLE_DATE_MENU).unwrap();
        let qs = items
            .iter()
            .find(|i| i.role == ROLE_QUICK_SETTINGS)
            .unwrap();
        assert_eq!(activities.r#box, PanelBox::Left);
        assert_eq!(date.r#box, PanelBox::Right);
        assert!(
            !items.iter().any(|i| i.r#box == PanelBox::Center),
            "the centre box is empty now that the clock moved right"
        );
        // The clock owns the output's right corner…
        assert_eq!(date.rect.loc.x + date.rect.size.w, ow);
        // …and the status indicators are to its left, not the other way round.
        assert_eq!(qs.rect.loc.x + qs.rect.size.w, date.rect.loc.x);
    }

    /// The dots are [`RoundedSolidRenderElement`]s now, one per workspace: the active one
    /// is a wide, full-opacity pill and every other a small, half-opacity circle
    /// (`panel.js WorkspaceIndicators._updateExpansion`). Pure geometry, no device.
    #[test]
    fn workspace_dots_expand_around_the_active_index() {
        let dots = workspace_dots(3, 1.);
        assert_eq!(dots.len(), 3, "one dot per workspace");

        let (active_rect, active_radius, active_color) = dots[1];
        assert_eq!(active_color[3], 1., "the active dot is full opacity");
        assert!(
            active_rect.size.w > DOT_DIAMETER,
            "the active dot is a pill wider than the base diameter: {}",
            active_rect.size.w
        );
        assert_eq!(active_rect.size.h, DOT_DIAMETER, "at full scale");
        // Half the height: a circle on a square dot, a stadium on the wide pill.
        assert_eq!(active_radius, active_rect.size.h / 2.);

        for (i, (rect, radius, color)) in dots.iter().enumerate() {
            if i == 1 {
                continue;
            }
            assert_eq!(color[3], INACTIVE_DOT_OPACITY as f32, "dot {i} is dimmed");
            assert_eq!(rect.size.w, rect.size.h, "dot {i} is a circle");
            assert!(
                rect.size.w < active_rect.size.w,
                "dot {i} is smaller than the active pill"
            );
            assert_eq!(*radius, rect.size.h / 2.);
        }

        // All of them share the vertical center of the bar.
        for (rect, _, _) in &dots {
            let cy = rect.loc.y + rect.size.h / 2.;
            assert!(
                (cy - panel_height() / 2.).abs() < 1e-9,
                "dot off the band: {cy}"
            );
        }

        // An out-of-range position parks the pill on the end dot rather than expanding
        // none of them (the clamp `WorkspaceState::active` used to carry).
        let past_the_end = workspace_dots(3, 9.);
        assert_eq!(past_the_end.len(), 3);
        assert_eq!(past_the_end[2].2[3], 1., "the last dot holds the wide pill");
        assert_eq!(workspace_dots(0, 0.).len(), 0);
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

    /// Mid-switch (a fractional position) the two straddled dots are each *partly* grown:
    /// strictly bigger and brighter than a resting inactive dot, strictly smaller and
    /// dimmer than a resting active one. Pins that the fractional path actually morphs
    /// them rather than snapping at the halfway mark. Pure geometry, no device.
    #[test]
    fn workspace_dots_morph_during_a_switch() {
        let rest = workspace_dots(2, 0.);
        let mid = workspace_dots(2, 0.5);

        let (active_w, active_a) = (rest[0].0.size.w, rest[0].2[3]);
        let (idle_w, idle_a) = (rest[1].0.size.w, rest[1].2[3]);

        for (i, (rect, _, color)) in mid.iter().enumerate() {
            assert!(
                idle_w < rect.size.w && rect.size.w < active_w,
                "mid-switch dot {i} width {} is not between {idle_w} and {active_w}",
                rect.size.w
            );
            assert!(
                idle_a < color[3] && color[3] < active_a,
                "mid-switch dot {i} opacity {} is not between {idle_a} and {active_a}",
                color[3]
            );
        }

        // And the morph is continuous: nudging the position nudges the geometry.
        let nudged = workspace_dots(2, 0.51);
        assert!(
            (nudged[0].0.size.w - mid[0].0.size.w).abs() > 1e-9,
            "a 0.01 step in position moved nothing — the dots are quantized"
        );
    }

    /// The clock button is sized from its advance box (see `clock_button_width`), which
    /// is constant across ticks only because the panel font's digits are tabular — that's
    /// what keeps the label from jittering left/right as the seconds change. It matters
    /// more now that the button is right-anchored: a wobbling advance would drag the
    /// label's left edge every second. Pins that invariant: if SansSerif ever resolves to
    /// a font with proportional digits this fails, flagging that the advance alone would
    /// no longer be steady.
    #[test]
    fn clock_advance_width_is_stable_across_seconds() {
        let px = font_px() as f32;
        let a = synoik_vk::text::measure_line_width("12:34:56", px);
        let b = synoik_vk::text::measure_line_width("12:34:07", px);
        let c = synoik_vk::text::measure_line_width("18:88:88", px);
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

    /// Drive the GPU bar chrome into an offscreen and read it back: the active
    /// Activities container pill on the left, bright clock glyph ink — and **nothing
    /// where the background used to be**.
    ///
    /// That last one is the invariant, not a detail: the bar background is a separate
    /// element now, and if it crept back into this bake the bake would stop being
    /// cacheable across an overview fade (its colour changes every frame), costing a GPU
    /// round trip per animation frame. Transparency here is what keeps it cached.
    /// Skips cleanly with no device.
    #[test]
    fn draws_a_bar_with_glyph_coverage() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_a_bar_with_glyph_coverage: no Vulkan device ({e})");
                return;
            }
        };
        use smithay::backend::renderer::Texture as _;

        let width_px = 400;
        let height_px = panel_height() as i32;
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
        assert!(
            !panel.button_containers(width_px as f64, ws).is_empty(),
            "the open overview must light the Activities pill, or the assertion below \
             that the bake does not contain it proves nothing",
        );
        let (mut tex, lead_px) =
            draw_label_texture(&mut vk, 1., "12:34").expect("clock label texture");
        let size = tex.size();

        // The texture is the *label*, not the bar. This is the invariant now: nothing that
        // belongs to another element can be in here, because there is no room for it. A
        // full-width bake is what made a clock tick cost a full-bar re-rasterization
        // (p50 60.7 ms on a live seat, once a minute, forever).
        assert!(
            size.w < width_px / 2,
            "the clock label must be label-sized, not bar-sized: {size:?} on a {width_px}px bar",
        );
        assert_eq!(size.h, height_px, "the label keeps the bar's full height");
        assert_eq!(
            lead_px, 0,
            "tabular digits start at their origin; a non-zero lead would mean the ink hangs \
             left of the anchor and `label_element` has to shift the element back",
        );

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let px_at = |x: i32, y: i32| -> [u8; 4] {
            let i = ((y * size.w + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };

        // The top-left corner is above the line box: background territory, and the label must
        // not paint it. The bar background and the container pills are separate elements —
        // their alpha animates, and an animated alpha in a bake costs a GPU round trip on every
        // frame of the fade. See `pill_element`.
        let corner = px_at(0, 0);
        assert_eq!(
            corner[3], 0,
            "the label bake must be transparent where the background element goes, got {corner:?}",
        );

        // Bright glyph ink somewhere (the clock text).
        let bright = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 40, "expected visible glyph ink, got {bright}");
    }

    /// **An overview fade must not re-bake the bar.** The background alpha animates every
    /// frame while the overview opens; the chrome does not. Since a bake is a GPU round
    /// trip — the single most expensive thing a frame does on this stack — sweeping the
    /// fade must reuse one cached bake and vary only the separate background element.
    ///
    /// Asserted on the bake counter, because pixels cannot see this: a bar re-baked every
    /// frame looks exactly like a cached one.
    #[test]
    fn an_overview_fade_reuses_one_bar_bake() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping an_overview_fade_reuses_one_bar_bake: no Vulkan device ({e})");
                return;
            }
        };
        let panel = test_panel();
        let output = Output::new(
            "panel-test".to_owned(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "synoik".to_owned(),
                model: "test".to_owned(),
                serial_number: "0".to_owned(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: Size::from((1920, 1080)),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        let icons = IconCache::new("Adwaita");

        // First render at rest warms the cache (and pays the one bake).
        let _ = panel.render(
            &mut vk,
            &output,
            ws,
            1.,
            0.,
            DrawCaches {
                icons: &icons,
                images: &crate::render_helpers::icon::ImageCache::new(),
            },
        );

        let before = crate::frame_log::bakes();
        let mut backgrounds = Vec::new();
        for step in 0..=10 {
            let fade = f64::from(step) / 10.;
            let elements = panel.render(
                &mut vk,
                &output,
                ws,
                1.,
                fade,
                DrawCaches {
                    icons: &icons,
                    images: &crate::render_helpers::icon::ImageCache::new(),
                },
            );
            let solid = elements.iter().find_map(|e| match e {
                PanelElement::Solid(s) => Some(s.color()),
                _ => None,
            });
            backgrounds.push(solid.map(|c| c.a()));
        }
        assert_eq!(
            crate::frame_log::bakes(),
            before,
            "sweeping the overview fade re-baked the bar instead of reusing the cache"
        );

        // And the background really did fade — otherwise the assertion above would pass
        // for the trivial reason that nothing was drawn.
        let first = backgrounds[0].expect("the desktop bar has a background");
        let last = backgrounds[10];
        assert!(
            (first - BAR_BG[3]).abs() < 0.01,
            "the desktop bar wash should be at its full alpha {}, got {first}",
            BAR_BG[3],
        );
        assert!(
            last.is_none_or(|a| a < 0.01),
            "the overview bar background should have faded out, got {last:?}"
        );
        assert!(
            backgrounds[5].is_some_and(|a| a > first * 0.2 && a < first * 0.8),
            "mid-fade should be partly transparent, got {:?}",
            backgrounds[5]
        );
    }

    /// **A clock tick re-bakes the clock, and nothing else.** The bar's three text labels used
    /// to share one bake the full width of the output, so the minute tick re-rasterized the
    /// whole bar. That is cheap warm (0.9 ms) and ruinous cold, which is what it always is at a
    /// once-a-minute cadence: a live seat measured p50 60.7 ms, p99 311 ms, and three to four
    /// dropped vblanks every minute on an idle desktop. See
    /// `docs/fork/foundation.md` §3.
    ///
    /// Counted on bakes rather than pixels, like [`an_overview_fade_reuses_one_bar_bake`]: a
    /// label re-baked needlessly renders identically to a cached one. Two ticks, because the
    /// first has to pay for the clock and the assertion is that it pays for *only* the clock —
    /// with a recording running, so there is a second label present to be spared.
    #[test]
    fn a_clock_tick_rebakes_only_the_clock() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping a_clock_tick_rebakes_only_the_clock: no Vulkan device ({e})");
                return;
            }
        };
        let mut panel = test_panel();
        let output = Output::new(
            "panel-test".to_owned(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "synoik".to_owned(),
                model: "test".to_owned(),
                serial_number: "0".to_owned(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: Size::from((1920, 1080)),
                refresh: 60_000,
            }),
            None,
            None,
            None,
        );
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        let icons = IconCache::new("Adwaita");
        let images = crate::render_helpers::icon::ImageCache::new();
        panel.set_keyboard_layout(Some("us".to_owned()));
        panel.set_recording(Some(panel.clock.now_unadjusted()));
        let render = |panel: &Panel, vk: &mut VulkanRenderer| {
            let _ = panel.render(
                vk,
                &output,
                ws,
                1.,
                0.,
                DrawCaches {
                    icons: &icons,
                    images: &images,
                },
            );
        };

        // Warm every label.
        render(&panel, &mut vk);
        let warm = crate::frame_log::bakes();
        render(&panel, &mut vk);
        assert_eq!(
            crate::frame_log::bakes(),
            warm,
            "a re-render with nothing changed must not bake at all",
        );

        // 12:00 → 12:01. Exactly one label's text changed.
        assert!(
            panel.update_clock_at(1_754_000_460),
            "the clock text must actually change, or this test proves nothing",
        );
        let before = crate::frame_log::bakes();
        render(&panel, &mut vk);
        assert_eq!(
            crate::frame_log::bakes(),
            before + 1,
            "a clock tick must re-bake the clock label alone — the recording and keyboard \
             labels did not change",
        );
    }

    /// The clock is vertically centered on its font *line-box* (the constant ascent+descent about
    /// the baseline), like St/Pango — not on its *ink*. The observable consequence, and the reason
    /// GNOME does it: the vertical position is baseline-stable — it does not depend on which glyphs
    /// happen to carry a descender. A clock reading "12:34 pm" (the `p` dips below the baseline)
    /// must sit at exactly the same `y` as "12:34"; ink centering would nudge the whole label up as
    /// the descender appears, jittering it vertically the way proportional digits jitter it
    /// horizontally. Skips with no device.
    #[test]
    fn clock_vertical_centering_is_baseline_stable() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping clock_vertical_centering_is_baseline_stable: no device ({e})");
                return;
            }
        };

        let scale = 2.;
        let height_px = to_physical_precise_round(scale, panel_height());
        let bold = TextStyle::new(FONT_PT).bold();

        let (plain, descend, ink_plain, ink_descend) = {
            let mut shaper = TextShaper::new(&mut vk, scale);
            let a = shaper.shape("12:34", bold).expect("shape");
            let b = shaper.shape("12:34 pm", bold).expect("shape");
            (
                a.line_box_centered_y(height_px),
                b.line_box_centered_y(height_px),
                a.ink_bounds(),
                b.ink_bounds(),
            )
        };

        assert_eq!(
            plain, descend,
            "line-box centering must place both labels at the same y ({plain} vs {descend})"
        );

        // Guard against a false pass: the two labels really do differ in ink (the `p`'s descender
        // grows the ink box), so an ink-centered origin *would* differ — the property is
        // non-trivial.
        let ink_y = |(_, iy, _, ih): (i32, i32, i32, i32)| (height_px - ih) / 2 - iy;
        assert_ne!(
            ink_y(ink_plain),
            ink_y(ink_descend),
            "test is only meaningful if ink centering would differ; ink boxes \
             {ink_plain:?} vs {ink_descend:?}"
        );
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

    /// The accessibility indicator's presence is a *predicate*, not a setting: gnome-shell
    /// shows it when `always-show-universal-access-status` is on **or** any a11y feature
    /// is enabled (`ATIndicator._syncMenuVisibility`, `js/ui/status/accessibility.js:90-97`).
    /// Its right-box slot is between `screenRecording` and `keyboard`
    /// (`js/ui/sessionMode.js:99`). Structural, no GPU.
    #[test]
    fn a11y_indicator_visibility_and_order() {
        use crate::gnome::A11yToggle;

        let clock = Clock::default();
        let mut panel = Panel::new(clock.clone(), Rc::new(RefCell::new(Config::default())));
        let ws = WorkspaceState {
            count: 1,
            active: 0,
        };
        let ow = 1920.;

        // A default profile has nothing enabled and no pin: no indicator, no re-set churn.
        assert!(!panel.set_a11y(A11ySettings::default()));
        assert!(panel.a11y_rect(ow).is_none());
        assert!(panel.items(ow, ws).iter().all(|i| i.role != ROLE_A11Y));

        // Any one enabled feature brings it out.
        let mut a11y = A11ySettings::default();
        a11y.set(A11yToggle::StickyKeys, true);
        assert!(panel.set_a11y(a11y));
        let items = panel.items(ow, ws);
        let at = items
            .iter()
            .find(|i| i.role == ROLE_A11Y)
            .expect("an enabled a11y feature shows the indicator");
        assert_eq!(at.r#box, PanelBox::Right);
        let center = Point::from((at.rect.loc.x + at.rect.size.w / 2., 16.));
        assert_eq!(panel.hit_test(center, ow, ws), Some(ROLE_A11Y));

        // Order: screenRecording | a11y | keyboard | quickSettings, each abutting the next.
        panel.set_keyboard_layout(Some("us".into()));
        panel.set_recording(Some(clock.now_unadjusted()));
        let items = panel.items(ow, ws);
        let rect_of = |role: &str| {
            items
                .iter()
                .find(|i| i.role == role)
                .unwrap_or_else(|| panic!("{role} missing"))
                .rect
        };
        let (rx, ax, kx, qx) = (
            rect_of(ROLE_SCREEN_RECORDING),
            rect_of(ROLE_A11Y),
            rect_of(ROLE_KEYBOARD),
            rect_of(ROLE_QUICK_SETTINGS),
        );
        assert!(
            (rx.loc.x + rx.size.w - ax.loc.x).abs() < 1e-6,
            "recording abuts a11y"
        );
        assert!(
            (ax.loc.x + ax.size.w - kx.loc.x).abs() < 1e-6,
            "a11y abuts keyboard"
        );
        assert!(
            (kx.loc.x + kx.size.w - qx.loc.x).abs() < 1e-6,
            "keyboard abuts QS"
        );

        // Large Text counts through its factor, not a flag (`accessibility.js:120-122`).
        let mut a11y = A11ySettings::default();
        a11y.set(A11yToggle::LargeText, true);
        assert!(panel.set_a11y(a11y));
        assert!(
            panel.a11y_rect(ow).is_some(),
            "Large Text shows the indicator"
        );

        // Turning everything off hides it again — the predicate is live.
        assert!(panel.set_a11y(A11ySettings::default()));
        assert!(panel.a11y_rect(ow).is_none());

        // The pin alone is enough, with nothing enabled.
        let mut a11y = A11ySettings::default();
        a11y.always_show = true;
        assert!(panel.set_a11y(a11y));
        assert!(
            panel.a11y_rect(ow).is_some(),
            "always-show-universal-access-status pins the indicator on"
        );
    }

    /// The keyboard input-source indicator shares the panel-button pill: it lights on
    /// hover and stays lit while its menu is up, just like the clock and quick-settings
    /// (`InputSourceIndicator extends PanelMenu.Button`, `js/ui/status/keyboard.js:875`).
    #[test]
    fn keyboard_indicator_wears_the_shared_pill() {
        let mut clock = Clock::default();
        let mut panel = Panel::new(clock.clone(), Rc::new(RefCell::new(Config::default())));
        let ws = WorkspaceState {
            count: 1,
            active: 0,
        };
        let ow = 1920.;
        assert!(panel.set_keyboard_layout(Some("us".into())));

        let kb = panel.keyboard_rect(ow).expect("keyboard indicator present");
        // Its pill is the same inset container the other buttons get.
        let pill = container_rect(kb);
        let has_kb_pill = |panel: &Panel| {
            panel
                .button_containers(ow, ws)
                .iter()
                .any(|(rect, _)| (rect.loc.x - pill.loc.x).abs() < 1e-6)
        };

        // Idle: no pill.
        assert!(!has_kb_pill(&panel), "idle keyboard button has no pill");

        // Hover lights it (settle past the 150ms fade).
        assert!(panel.set_hovered_role(Some(ROLE_KEYBOARD)));
        clock.set_unadjusted(clock.now_unadjusted() + Duration::from_millis(500));
        assert!(
            has_kb_pill(&panel),
            "hovered keyboard button lights its pill"
        );

        // Opening its menu keeps it lit even without hover (checked state).
        assert!(panel.set_hovered_role(None));
        assert!(panel.set_open_menu(Some(ROLE_KEYBOARD)));
        clock.set_unadjusted(clock.now_unadjusted() + Duration::from_millis(500));
        assert!(
            has_kb_pill(&panel),
            "keyboard button stays lit while its menu is open",
        );

        // Closing the menu drops the pill again.
        assert!(panel.set_open_menu(None));
        clock.set_unadjusted(clock.now_unadjusted() + Duration::from_millis(500));
        assert!(!has_kb_pill(&panel), "closed menu clears the pill");
    }

    /// The recording pill draws red (`#c01c28`) with its white `M:SS` label on top —
    /// and the two now live in different textures, so this checks both halves line up:
    /// the pill comes from [`pill_texture`], the label from the chrome bake.
    /// Skips with no device.
    #[test]
    fn the_recording_pill_is_red_under_its_label() {
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

        // A red pill on the right with a label just inside its left padding.
        let pill = Rectangle::new(Point::from((300., 3.)), Size::from((90., 26.)));

        // The pill itself, baked at full alpha into its own texture.
        let mut cache = widget::BakeCache::new();
        let mut pill_tex = pill_texture(&mut vk, &mut cache, scale, pill.size, R1_BG)
            .expect("pill texture")
            .texture()
            .clone();
        let pill_size = pill_tex.size();
        let fb = vk.bind(&mut pill_tex).expect("bind pill for readback");
        let mapping = vk
            .copy_framebuffer(
                &fb,
                Rectangle::<i32, BufferCoord>::from_size(pill_size),
                Fourcc::Abgr8888,
            )
            .expect("copy_framebuffer");
        let pill_px = vk.map_texture(&mapping).expect("map_texture").to_vec();
        let red = pill_px
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] < 90 && p[2] < 90 && p[3] > 200)
            .count();
        assert!(red > 50, "expected a red recording pill, got {red} red px");
        // A stadium, not a rectangle: the corner is outside the rounding.
        let corner = &pill_px[0..4];
        assert_eq!(corner[3], 0, "the pill must be rounded, got {corner:?}");
        drop(fb);

        // The M:SS label, its own texture, composited over the pill.
        let (mut tex, _) = draw_label_texture(&mut vk, scale, "0:05").expect("label texture");
        let size = tex.size();
        // It must fit inside the pill it draws over, or it is not a label texture.
        assert!(
            size.w < to_physical_precise_round::<i32>(scale, pill.size.w),
            "the M:SS label must fit its pill: {size:?} in a {:?} pill",
            pill.size,
        );

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // Readback is [R, G, B, A] per pixel.
        let mut ink = 0usize;
        let mut label_red = 0usize;
        for p in pixels.chunks_exact(4) {
            if p[0] > 150 && p[1] < 90 && p[2] < 90 && p[3] > 200 {
                label_red += 1;
            }
            if p[0] > 200 && p[1] > 200 && p[2] > 200 {
                ink += 1;
            }
        }
        assert!(ink > 5, "expected white M:SS label ink, got {ink} px");
        assert_eq!(
            label_red, 0,
            "the pill is its own element now; the label bake must not paint it",
        );
    }

    /// **A container fade must not re-bake the pill.** The whole point of moving the
    /// button containers out of the chrome is that their alpha is a composite-time
    /// property: one bake per shape has to serve an entire fade.
    ///
    /// This is the assertion that would have caught the panel's per-frame bake at the
    /// unit level. Like [`an_overview_fade_reuses_one_bar_bake`], it has to be made on
    /// the bake counter — a pill re-baked every frame renders identically to a cached
    /// one.
    #[test]
    fn a_container_fade_reuses_one_pill_bake() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping a_container_fade_reuses_one_pill_bake: no Vulkan device ({e})");
                return;
            }
        };

        let mut cache = widget::BakeCache::new();
        let rect = Rectangle::new(Point::from((0., 3.)), Size::from((90., 26.)));

        // Warm the cache, and pay the one bake this shape is allowed.
        let _ = pill_element(&mut vk, &mut cache, 1., rect, [1., 1., 1., 0.]).expect("pill");

        let before = crate::frame_log::bakes();
        for step in 0..=10 {
            let alpha = step as f32 / 10.;
            pill_element(&mut vk, &mut cache, 1., rect, [1., 1., 1., alpha]).expect("pill");
        }
        assert_eq!(
            crate::frame_log::bakes(),
            before,
            "sweeping a container fade re-baked the pill instead of compositing it at alpha",
        );

        // A different *shape* must still bake — otherwise the assertion above would
        // hold for the trivial reason that nothing is being cached by shape at all.
        let wider = Rectangle::new(rect.loc, Size::from((120., 26.)));
        pill_element(&mut vk, &mut cache, 1., wider, [1., 1., 1., 1.]).expect("pill");
        assert_eq!(
            crate::frame_log::bakes(),
            before + 1,
            "a pill of a new shape has to be baked",
        );
    }

    /// The container pill still lands where it did, at the alpha it did, once the
    /// panel's elements are composited.
    ///
    /// Moving the pill out of the chrome bake moved it from being *painted* at exact
    /// physical coordinates inside one texture to being *placed* as its own element at
    /// a logical location — which is where a fractional scale could shift it by a
    /// half-pixel or drop it entirely. The unit tests around it check the pill texture
    /// and its caching; only compositing checks that it is in the right place, and at a
    /// non-integer scale where a placement bug actually shows.
    #[test]
    fn the_composited_container_pill_keeps_its_place_and_alpha() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping the_composited_container_pill_keeps_its_place_and_alpha: {e}");
                return;
            }
        };

        let scale = 2.25;
        let (w, h) = (600., panel_height());
        let output = Output::new(
            "pill-test".to_owned(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "synoik".to_owned(),
                model: "test".to_owned(),
                serial_number: "0".to_owned(),
            },
        );
        output.change_current_state(
            Some(smithay::output::Mode {
                size: Size::from((to_physical_precise_round(scale, w), 1080)),
                refresh: 60_000,
            }),
            None,
            Some(smithay::output::Scale::Fractional(scale)),
            None,
        );

        let ws = WorkspaceState {
            count: 3,
            active: 0,
        };
        let mut clock = Clock::default();
        let mut panel = Panel::new(clock.clone(), Rc::new(RefCell::new(Config::default())));
        // Overview open → the Activities pill is checked; settle past the 150ms fade so
        // it sits at its full target alpha rather than somewhere mid-ramp.
        panel.set_overview_open(true);
        clock.set_unadjusted(clock.now_unadjusted() + Duration::from_millis(500));

        let icons = IconCache::new("Adwaita");
        let elements = panel.render(
            &mut vk,
            &output,
            ws,
            0.,
            0.,
            DrawCaches {
                icons: &icons,
                images: &crate::render_helpers::icon::ImageCache::new(),
            },
        );
        assert!(
            elements
                .iter()
                .any(|e| matches!(e, PanelElement::Texture(_))),
            "expected the panel to contribute textures",
        );

        let size = Size::<i32, Physical>::from((
            to_physical_precise_round(scale, w),
            to_physical_precise_round(scale, h),
        ));
        let pixels = crate::render_helpers::render_to_vec(
            &mut vk,
            size,
            smithay::utils::Scale::from(scale),
            Transform::Normal,
            Fourcc::Abgr8888,
            // `render_elements` draws in iterator order, i.e. back-to-front, while a
            // panel element list is front-to-back (index 0 topmost) — hence the reverse.
            // Feeding it unreversed paints the opaque bar background over everything and
            // reads as "the pill did not draw".
            elements.into_iter().rev(),
        )
        .expect("composite the panel");

        let px_at = |x: i32, y: i32| -> [u8; 4] {
            let i = ((y * size.w + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // Inside the Activities pill and above the dot band — the same spot the bake
        // test used to sample, in physical pixels at this scale.
        let inside = px_at(
            to_physical_precise_round(scale, 17.),
            to_physical_precise_round(scale, 6.),
        );
        // The bar's background is a *black* wash at [`BAR_BG`]'s alpha over a blur of nothing (this
        // composites the panel alone, onto a transparent clear), so the pill's white fill shows as
        // a premultiplied grey *level* independent of what is behind: white at α0.28 is 0.28·255 ≈
        // 71 in every channel. Only the alpha carries the wash — 0.28 over 0.4 ≈ 0.57.
        let expect_a = (BAR_BG[3] + 0.28 * (1. - BAR_BG[3])) * 255.;
        assert!(
            (inside[3] as f32 - expect_a).abs() <= 2.
                && inside[0] == inside[1]
                && inside[1] == inside[2]
                && (60..=85).contains(&inside[0]),
            "expected the checked Activities pill fill (grey ~71, alpha ~{expect_a:.0} over the \
             translucent bar) at 17,6, got {inside:?}",
        );

        // Outside the pill's right edge the bar is bare again — so the assertion above is
        // about a pill, not about something that filled the whole bar.
        let beyond = px_at(
            to_physical_precise_round(scale, w - 8.),
            to_physical_precise_round(scale, 6.),
        );
        assert_eq!(
            [beyond[0], beyond[1], beyond[2]],
            [0, 0, 0],
            "the pill must not extend past the Activities button, got {beyond:?}",
        );
    }
}
