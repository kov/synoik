//! The quick-settings menu (the right-hand panel status area's popover).
//!
//! A fork-owned port of gnome-shell's `js/ui/quickSettings.js`: a **system row** at
//! the top (gnome-shell's `SystemItem`, which `panel.js` adds first) above a
//! two-column grid of [`QuickToggle`]-style tiles. The grid leads with a live
//! **Network** tile (wired/Wi-Fi state from the NetworkManager watcher, accent when
//! connected — a click opens network settings), then gsettings-backed toggles —
//! **Dark Style**, **Do Not Disturb**, **Night Light**. The system
//! row has a far-left battery **pill** (icon + percentage, only with a battery,
//! like `PowerToggle`), then screenshot / settings / lock / power. Each tile shows
//! a symbolic icon and a label and flips its gsettings key on click; screenshot
//! opens the interactive UI and the rest spawn the canonical session command.
//!
//! The chrome (menu bg + tile/pill backgrounds + labels) is drawn into one
//! offscreen `VkTexture`: `clear` for the menu fill, `render_rounded_rect` for the
//! pill-shaped tile and battery-pill backgrounds (gnome-shell's quick toggles use
//! `$forced_circular_radius`), and `render_glyphs` for the labels. The **icons are
//! composited as separate elements on top** from the shared [`IconCache`] —
//! symbolic SVGs recolored to the fore/back color of their slot.
//!
//! A **volume slider** (gnome-shell's output `.quick-slider`) sits between the system
//! row and the tile grid when a sink is present: a mute icon-button plus a draggable
//! track bound to the default sink's volume.
//!
//! Deferred vs gnome-shell: the brightness slider, the bluetooth toggle, the
//! per-toggle detail sub-menus (the Network tile opens settings instead of an
//! in-menu enable/disable + connection list), and SSID/connection-name labels. The
//! self-contained tiles, the Network status tile, the system row, the battery
//! pill, and the volume slider are here.

use std::cell::RefCell;
use std::collections::HashMap;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture as _,
};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::audio::{AudioStatus, MicStatus, SinkList, SourceList};
use crate::gnome::QuickToggles;
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::system_status::{
    self, AirplaneStatus, BatteryStatus, NetworkStatus, PowerProfileStatus,
};
use crate::ui::popover::PopoverAction;
use crate::utils::to_physical_precise_round;

// Geometry, logical px (grounded in gnome-shell-sass quick-settings proportions).
/// `.quick-settings` padding is `$base_padding * 3` (`$base_padding: 6px` → 18px) — the uniform
/// outer margin every element insets from (`_quick-settings.scss:2`).
const PAD: f64 = 18.;
/// `.quick-toggle` is a fixed `12em` (`min/max-width`, `_quick-settings.scss:17-18`). `1em` is
/// `$base_font_size` (11pt), so 12em = `12 * pt_to_px(11)` ≈ 176px — wider than the old 150, which
/// cramped two-line tiles (Power Mode) and made the whole menu narrower than gnome-shell's.
const TILE_W: f64 = 12.0 * crate::ui::pt_to_px(11.0);
const TILE_H: f64 = 56.;
const TILE_GAP: f64 = 8.;
const COLS: usize = 2;
/// Icon size inside a tile, and its inset from the tile's left edge.
const TILE_ICON: f64 = 16.;
const TILE_ICON_INSET: f64 = 12.;
/// Tile-title / battery-percentage font size, logical px. GNOME's `.quick-toggle-title`
/// (and the power toggle's percentage `title`) is `%heading` = **11pt, weight 700**
/// (`gnome-shell-sass/_common.scss`), drawn bold — not regular weight, which reads too
/// light/small.
const LABEL_PX: f64 = crate::ui::pt_to_px(11.);

/// A tile subtitle's font size — gnome-shell's `.quick-toggle-subtitle` is `%caption` (9pt),
/// regular weight (`gnome-shell-sass/widgets/_quick-settings.scss`). Only Power Mode uses a
/// subtitle so far.
const SUBTITLE_PX: f64 = crate::ui::pt_to_px(9.);

/// Half the vertical gap between a two-line tile's title and subtitle line centers (each is offset
/// this far from the tile's vertical center). Keeps the 11pt title + 9pt subtitle from overlapping
/// inside `TILE_H` without growing the tile (gnome-shell's tiles don't grow for a subtitle either).
const SUBTITLE_GAP: f64 = 9.;

/// The system row (Settings on the left, Lock/Power on the right) sits at the
/// **top** of the menu, above the tile grid — like gnome-shell's `SystemItem`,
/// which `panel.js` adds first (`_addItemsBefore(this._system…)`).
const SYS_H: f64 = 44.;
/// Symbolic-icon size inside a system button. gnome-shell's `.icon-button` uses
/// `icon-size: $scalable_icon_size` = 16px (`_buttons.scss`).
const SYS_ICON: f64 = 16.;
/// Diameter of a system button's circular background disc, and its hit target. The
/// `.icon-button` is the 16px icon plus `$scaled_padding * 2` = 12px padding on each
/// side (`_buttons.scss`) → 40px.
const SYS_HIT: f64 = 40.;
/// Gap between adjacent system-button discs: `$base_padding * 2` = 12px, the
/// `.quick-settings-system-item` box spacing (`_quick-settings.scss`).
const SYS_GAP: f64 = 12.;
/// Advance between adjacent disc centers: one disc plus the inter-disc gap.
const SYS_ADVANCE: f64 = SYS_HIT + SYS_GAP;
/// The battery pill (gnome-shell's `PowerToggle`): a wide item at the far left of
/// the system row showing the battery icon + percentage, only when a battery is
/// present. Clicking it opens power settings.
const PILL_W: f64 = 96.;
const PILL_ICON_INSET: f64 = 12.;

/// The volume slider row (gnome-shell's `.quick-slider`): a full-width row between the
/// system row and the tile grid with a mute icon-button at the left and the slider
/// track filling the rest. Height is the `.icon-button` disc; the track is a thin
/// trough with an accent-filled portion and a round handle (`_slider.scss`).
const SLIDER_H: f64 = 40.;
const SLIDER_ICON: f64 = 16.;
/// Slider handle diameter (`$slider_size` = 16px) and trough thickness
/// (`-barlevel-height` = 4px), logical px.
const SLIDER_HANDLE: f64 = 16.;
const SLIDER_TROUGH: f64 = 4.;
/// Track background (`transparentize($fg_color, .9)`) and filled portion (accent).
const SLIDER_TROUGH_BG: [f32; 4] = [1., 1., 1., 0.1];
// The slider's mute-button disc uses the same off-tile background as the tiles.

/// Outer radius of the menu panel, logical px. gnome-shell 50.1's `.quick-settings`
/// uses `border-radius: $modal_radius * 2.25` = `(8*2)*2.25` = 36px
/// (`gnome-shell-sass/_common.scss`, `widgets/_quick-settings.scss`). All content is
/// inset by `PAD` and self-rounded, so this arc never clips a tile or the pill.
const MENU_RADIUS: f64 = 36.;

const MENU_BG: [f32; 4] = [0.12, 0.12, 0.12, 1.];
/// Fully-transparent clear for the menu offscreen, so the rounded MENU_BG fill leaves
/// the four outer corners transparent (they blend to whatever is beneath the popover).
const TRANSPARENT: [f32; 4] = [0., 0., 0., 0.];
const TILE_OFF: [f32; 4] = [0.24, 0.24, 0.24, 1.];
/// Hover highlight: an additive white wash painted over a control's existing
/// background, behind its glyphs (GNOME raises a button's fg-wash by ~0.10 on
/// `:hover`, `_message-list.scss:72-75`; quick toggles use `button(hover)`, a
/// lightened bg). For flat detail rows with no base bg this is the whole
/// indication. Subtle by design; tune live.
const HOVER_WASH: [f32; 4] = [1., 1., 1., 0.1];
/// Text/icon on an inactive (dark) tile.
const FG_OFF: [f32; 4] = [1., 1., 1., 1.];
/// Text/icon on an active (accent) tile. GNOME's `-st-accent-fg-color` is hardcoded white
/// (`#ffffff`) for every accent — including the light ones like yellow — so the toggled-on
/// label and symbolic icon are white, not dark (st-theme-context.c:41, GNOME 50.1).
const FG_ON: [f32; 4] = [1., 1., 1., 1.];
const SYS_FG: [f32; 4] = [0.9, 0.9, 0.9, 1.];

/// The expand-arrow half of a menu-bearing tile (gnome-shell's `.quick-toggle-menu-button`):
/// a right-edge, full-height region carrying a `go-next-symbolic` arrow that opens the tile's
/// detail view. gnome-shell sizes it to the 16px icon plus `.icon-button` padding.
const ARROW_W: f64 = 44.;
const ARROW_ICON: f64 = 16.;
const ARROW_ICONS: &[&str] = &["go-next-symbolic", "pan-end-symbolic"];
/// The trailing check on the selected detail row (gnome-shell's `Ornament.CHECK`). Its native
/// `ornament-check-symbolic` ships only in gnome-shell's gresource, invisible to our
/// theme-directory icon cache — `object-select-symbolic` is the Adwaita equivalent that actually
/// resolves.
const CHECK_ICONS: &[&str] = &["object-select-symbolic", "emblem-ok-symbolic"];
/// The 1px divider between a menu tile's toggle-half and its arrow-half
/// (`.quick-toggle-separator`); a faint line readable on both the off and accent backgrounds.
const SEPARATOR_W: f64 = 1.;
const SEPARATOR_COLOR: [f32; 4] = [1., 1., 1., 0.22];

/// The in-menu detail view (gnome-shell's `QuickToggleMenu`): a rounded card pinned directly
/// **below its owner's row**, holding a header (icon + title) over a list of action rows.
/// Opening it grows the menu; the rows below the owner shift down by the card's block height.
/// v1 diverges from gnome-shell in three deferred ways: no slide-down height animation
/// (instant grow), no dimming of the rest of the menu, and no per-row hover highlight (the menu
/// has no pointer-motion routing yet).
const DETAIL_MARGIN: f64 = 12.; // `.quick-toggle-menu { margin: $base_padding*2 0 0 }`
const DETAIL_RADIUS: f64 = 24.; // `%card` → `$base_border_radius * 3`
const DETAIL_PAD: f64 = 10.;
const DETAIL_HEADER_H: f64 = 32.;
const DETAIL_HEADER_ICON: f64 = 24.; // `$medium_scalable_icon_size`
const DETAIL_HEADER_INSET: f64 = 10.;
const DETAIL_HEADER_GAP: f64 = 8.;
const DETAIL_ROW_H: f64 = 36.;
const DETAIL_ROW_GAP: f64 = 2.;
const DETAIL_ROW_INSET: f64 = 12.;
/// Extra space above a row that follows a group separator (e.g. the machine-power vs session
/// split in the shutdown menu). v1 renders the split as spacing rather than a drawn rule.
const DETAIL_SEP_EXTRA: f64 = 8.;
/// Detail-card surface (a touch lighter than `MENU_BG`, gnome-shell's `%card`).
const CARD_BG: [f32; 4] = [0.18, 0.18, 0.18, 1.];
/// Header-title / row-label font size, logical px. Rows are regular weight (`.popup-menu-item`),
/// the header title is bold (`%title_3`).
const DETAIL_TITLE_PX: f64 = crate::ui::pt_to_px(11.);
const DETAIL_ROW_PX: f64 = crate::ui::pt_to_px(11.);

/// One actionable row in a detail view (gnome-shell's `addAction` items). `separator_before`
/// opens a visual group break above the row (the shutdown menu's power/session split).
struct DetailRow {
    label: String,
    /// Optional leading symbolic-icon candidates (empty = label-only, like the shutdown rows).
    icons: Vec<String>,
    action: PopoverAction,
    separator_before: bool,
    /// Whether this row is the current selection (a trailing check, gnome-shell's
    /// `Ornament.CHECK`).
    selected: bool,
}

/// Who owns the currently-open detail view. Keyed by **identity**, not a grid index, so it also
/// names the system-row power button and the volume slider (neither of which a grid index can
/// name), and never desyncs if `GRID` is reordered. See gnome-shell's `QuickMenuToggle` /
/// `ShutdownItem` / `QuickSlider`, all `hasMenu` items sharing the same `QuickToggleMenu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailOwner {
    /// The Network grid tile's detail view (its expand-arrow half).
    Network,
    /// The system-row power button's session submenu (gnome-shell's `ShutdownItem`).
    Power,
    /// The Power Mode grid tile's profile picker (gnome-shell's `PowerProfilesToggle` menu).
    PowerProfile,
    /// The volume slider's output-device picker (gnome-shell's `OutputStreamSlider` device menu).
    Output,
    /// The mic slider's input-device picker (gnome-shell's `InputStreamSlider` device menu).
    Input,
}

/// The most device rows a picker renders (Fable): the card grows with the device count and the
/// popover has no scrolling, so cap the list to keep the trailing "Sound Settings" row on-screen.
/// Beyond this the extra devices are dropped (a rare config — many null-sinks or a big HDMI/BT
/// fleet). Shared by the output (sink) and input (source) pickers.
const MAX_DEVICE_ROWS: usize = 6;

/// One of the two stacked volume sliders. gnome-shell adds the output slider then the input
/// (microphone) slider as consecutive quick-settings items, so the mic stacks directly below the
/// output slider, above the tile grid (`volume.js` `InputIndicator` push order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slider {
    Output,
    Mic,
}

impl Slider {
    /// The detail picker this slider's arrow opens.
    fn owner(self) -> DetailOwner {
        match self {
            Slider::Output => DetailOwner::Output,
            Slider::Mic => DetailOwner::Input,
        }
    }
}

/// Which slider rows are present, so the pure geometry can place both. The output slider shows when
/// a sink is bound (`audio.is_some()`); the mic slider only while recording with a bound source
/// (gnome-shell's `_shouldBeVisible = stream != null && recording`, `volume.js:429`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sliders {
    output: bool,
    mic: bool,
}

impl Sliders {
    /// Count of present slider rows (0, 1, or 2).
    fn count(self) -> usize {
        self.output as usize + self.mic as usize
    }

    fn present(self, sl: Slider) -> bool {
        match sl {
            Slider::Output => self.output,
            Slider::Mic => self.mic,
        }
    }

    /// This slider's vertical slot among the present sliders, top-down: Output is always slot 0;
    /// Mic follows the output slider (slot 1) when present, else takes slot 0.
    fn slot(self, sl: Slider) -> usize {
        match sl {
            Slider::Output => 0,
            Slider::Mic => self.output as usize,
        }
    }
}

/// A spawn `DetailRow` from a command's words.
fn spawn_row(label: &str, cmd: &[&str], separator_before: bool) -> DetailRow {
    DetailRow {
        label: label.to_string(),
        icons: Vec::new(),
        action: PopoverAction::Spawn(cmd.iter().map(|s| s.to_string()).collect()),
        separator_before,
        selected: false,
    }
}

impl DetailOwner {
    /// The header shown at the top of the detail card: symbolic-icon candidates + title, given
    /// the live state the owner reflects.
    fn header(self, network: NetworkStatus) -> (Vec<String>, String) {
        match self {
            DetailOwner::Network => (network_icons(network), network_label(network).to_string()),
            // gnome-shell's `ShutdownItem` menu header (`status/system.js`).
            DetailOwner::Power => (
                vec!["system-shutdown-symbolic".to_string()],
                "Power Off".to_string(),
            ),
            // gnome-shell's `menu.setHeader('power-profile-balanced-symbolic', 'Power Mode')`.
            DetailOwner::PowerProfile => (
                vec!["power-profile-balanced-symbolic".to_string()],
                "Power Mode".to_string(),
            ),
            // gnome-shell's output slider header (`volume.js:314`).
            DetailOwner::Output => (
                vec!["audio-headphones-symbolic".to_string()],
                "Sound Output".to_string(),
            ),
            // gnome-shell's input slider header (`volume.js:391`).
            DetailOwner::Input => (
                vec!["audio-input-microphone-symbolic".to_string()],
                "Sound Input".to_string(),
            ),
        }
    }

    /// The action rows, top to bottom, given the live state.
    fn rows(
        self,
        network: NetworkStatus,
        sink_list: &SinkList,
        source_list: &SourceList,
        power: &PowerProfileStatus,
    ) -> Vec<DetailRow> {
        match self {
            // v1 Network detail: a single entry point to the full settings (the in-menu
            // enable/disable toggle and the Wi-Fi connection list are Q6, needing NM writes).
            DetailOwner::Network => {
                let _ = network;
                vec![DetailRow {
                    label: "Network Settings".to_string(),
                    icons: Vec::new(),
                    action: PopoverAction::Spawn(
                        ["gnome-control-center", "network"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                    separator_before: false,
                    selected: false,
                }]
            }
            // gnome-shell's shutdown submenu, in its two groups: machine-power (Suspend / Restart /
            // Power Off) then, past a separator, the session group (Log Out). The `…` marks the
            // ones that go through a confirmation dialog; Suspend acts immediately. Restart / Power
            // Off / Log Out drive our own gnome-session handshake (EndSessionDialog); Suspend goes
            // straight to logind via systemctl. Switch User is deferred (needs a greeter jump).
            DetailOwner::Power => vec![
                spawn_row("Suspend", &["systemctl", "suspend"], false),
                spawn_row("Restart…", &["gnome-session-quit", "--reboot"], false),
                spawn_row("Power Off…", &["gnome-session-quit", "--power-off"], false),
                spawn_row("Log Out…", &["gnome-session-quit", "--logout"], true),
            ],
            // gnome-shell's power-profile list: one row per KNOWN profile (already reversed to
            // performance→power-saver), the active one carrying a trailing check, clicking sets it;
            // then a separator + a "Power Settings" entry point (`powerProfiles.js:75-81`). Kept
            // text+check like the device pickers (no per-row profile icon, our accepted
            // simplification).
            DetailOwner::PowerProfile => {
                let mut rows: Vec<DetailRow> = power
                    .available
                    .iter()
                    .map(|profile| DetailRow {
                        label: profile.name().to_string(),
                        icons: Vec::new(),
                        action: PopoverAction::SetPowerProfile(profile.id().to_string()),
                        separator_before: false,
                        selected: power.active == profile.id(),
                    })
                    .collect();
                rows.push(spawn_row(
                    "Power Settings",
                    &["gnome-control-center", "power"],
                    true,
                ));
                rows
            }
            // gnome-shell's output device list: one row per sink (label = description; the current
            // default carries a trailing check; clicking sets it default via a metadata write),
            // then a separator + a "Sound Settings" entry point (`volume.js:80-82,126-165`).
            DetailOwner::Output => {
                let mut rows: Vec<DetailRow> = sink_list
                    .sinks
                    .iter()
                    .take(MAX_DEVICE_ROWS)
                    .map(|sink| DetailRow {
                        label: sink.description.clone(),
                        icons: Vec::new(),
                        action: PopoverAction::SetDefaultSink(sink.name.clone()),
                        separator_before: false,
                        selected: sink_list.default_name.as_deref() == Some(sink.name.as_str()),
                    })
                    .collect();
                rows.push(spawn_row(
                    "Sound Settings",
                    &["gnome-control-center", "sound"],
                    true,
                ));
                rows
            }
            // The input mirror of Output: one row per source, then "Sound Settings".
            DetailOwner::Input => {
                let mut rows: Vec<DetailRow> = source_list
                    .sources
                    .iter()
                    .take(MAX_DEVICE_ROWS)
                    .map(|source| DetailRow {
                        label: source.description.clone(),
                        icons: Vec::new(),
                        action: PopoverAction::SetDefaultSource(source.name.clone()),
                        separator_before: false,
                        selected: source_list.default_name.as_deref() == Some(source.name.as_str()),
                    })
                    .collect();
                rows.push(spawn_row(
                    "Sound Settings",
                    &["gnome-control-center", "sound"],
                    true,
                ));
                rows
            }
        }
    }

    /// The per-row `separator_before` flags, top to bottom — the card's row *shape*, derived purely
    /// from the device count (no label/state), so the geometry can size the card without building
    /// rows. MUST match `rows()`'s length + separators (a debug_assert checks it at the draw/hit
    /// sites). `device_count` is ignored by the fixed owners; it's the sink count for Output, the
    /// source count for Input, and the profile count for PowerProfile.
    fn row_shape(self, device_count: usize) -> Vec<bool> {
        match self {
            DetailOwner::Network => vec![false],
            DetailOwner::Power => vec![false, false, false, true],
            // N device/profile rows, then a trailing settings row past a separator.
            DetailOwner::Output | DetailOwner::Input | DetailOwner::PowerProfile => {
                let mut shape = vec![false; device_count.min(MAX_DEVICE_ROWS)];
                shape.push(true);
                shape
            }
        }
    }

    /// The card's logical height: top pad + header + gap + rows + separators + bottom pad.
    fn detail_height(self, device_count: usize) -> f64 {
        let shape = self.row_shape(device_count);
        let rows = shape.len() as f64;
        let seps = shape.iter().filter(|&&s| s).count() as f64;
        DETAIL_PAD
            + DETAIL_HEADER_H
            + DETAIL_HEADER_GAP
            + rows * DETAIL_ROW_H
            + (rows - 1.).max(0.) * DETAIL_ROW_GAP
            + seps * DETAIL_SEP_EXTRA
            + DETAIL_PAD
    }

    /// The natural (pre-shift) y of the bottom edge of the owner's row — where the detail card is
    /// pinned directly below (gnome-shell binds the menu container's Y to the source actor).
    fn anchor_row_bottom(self, sliders: Sliders) -> f64 {
        match self {
            DetailOwner::Network => {
                let row = (network_index() / COLS) as f64;
                grid_top(sliders) + (row + 1.) * TILE_H + row * TILE_GAP
            }
            // The Power Mode tile is the first appended conditional at the constant
            // `power_profile_index()`, so its card pins below that row (same formula as Network).
            DetailOwner::PowerProfile => {
                let row = (power_profile_index() / COLS) as f64;
                grid_top(sliders) + (row + 1.) * TILE_H + row * TILE_GAP
            }
            // The power button lives in the top system row, so its detail pins right below it —
            // above the sliders and the whole grid, which shift down.
            DetailOwner::Power => PAD + SYS_H,
            // The picker pins below its slider's row (each picker is open only while its slider
            // exists — `normalize_expanded` guarantees it). Derived from the slider's slot so the
            // Input anchor is right whether or not the output slider is present.
            DetailOwner::Output => slider_row_y(Slider::Output, sliders) + SLIDER_H,
            DetailOwner::Input => slider_row_y(Slider::Mic, sliders) + SLIDER_H,
        }
    }
}

/// The grid slot the Network tile occupies (its detail view anchors below this row). Derived by
/// identity over [`BASE_GRID`] — the conditional tiles are only ever appended (see [`grid`]), so
/// Network's index is stable regardless of whether they show.
fn network_index() -> usize {
    BASE_GRID
        .iter()
        .position(|t| matches!(t, GridTile::Network))
        .unwrap_or(0)
}

/// The grid slot the Power Mode tile occupies when shown. It's the **first** conditional tile
/// appended after [`BASE_GRID`] (before Airplane — see [`grid`]), so its index is the constant
/// `BASE_GRID.len()` whenever present; its detail view anchors below this row. The append order is
/// load-bearing here and pinned by a debug_assert at the hit/render sites.
fn power_profile_index() -> usize {
    BASE_GRID.len()
}

/// The menu-local layout context: everything the pure geometry functions need to place elements,
/// including the vertical shift a below-the-owner-row detail view imposes. Threaded through every
/// geometry fn so hit-testing and rendering share one source of truth for the shift — and, via
/// `sink_count`, size a dynamic detail view (the output picker's per-sink rows) identically to what
/// the renderer draws.
#[derive(Debug, Clone, Copy)]
struct Layout {
    sliders: Sliders,
    expanded: Option<DetailOwner>,
    /// Number of output sinks, input sources, and known power profiles — the state a detail card's
    /// *size* depends on (the picker row counts) and the state each picker arrow is gated on.
    /// Fixed owners ignore them.
    sink_count: usize,
    source_count: usize,
    profile_count: usize,
    /// The slider being dragged and the device count frozen at drag start, so a device hot-plug
    /// mid-drag can't add/remove that slider's picker arrow (which would resize the track and
    /// remap `volume_from_x`, snapping the level). Scoped to the arrow/track only — the detail
    /// card still sizes from the live count. `None` when not dragging.
    drag: Option<(Slider, usize)>,
    /// The number of grid tiles (4, or 5 with the airplane tile shown) — the grid's row count, and
    /// thus the menu height, depends on it.
    grid_len: usize,
}

impl Layout {
    /// The device count that a slider's picker arrow reflects: the frozen count while this slider
    /// is being dragged, else the live count (sinks for Output, sources for Mic).
    fn arrow_count(self, sl: Slider) -> usize {
        match self.drag {
            Some((dragged, frozen)) if dragged == sl => frozen,
            _ => match sl {
                Slider::Output => self.sink_count,
                Slider::Mic => self.source_count,
            },
        }
    }

    /// The live device count feeding an owner's detail card (never frozen — the card always sizes
    /// to the real rows). Fixed owners return 0 (ignored by their `row_shape`).
    fn owner_device_count(self, owner: DetailOwner) -> usize {
        match owner {
            DetailOwner::Output => self.sink_count,
            DetailOwner::Input => self.source_count,
            DetailOwner::PowerProfile => self.profile_count,
            _ => 0,
        }
    }

    /// The detail card's `(natural insert y, block height)` when a view is open. The block height
    /// is the card plus its top margin — exactly how far the rows below the owner shift down, and
    /// how much taller the menu grows.
    fn detail_block(self) -> Option<(f64, f64)> {
        let owner = self.expanded?;
        Some((
            owner.anchor_row_bottom(self.sliders),
            DETAIL_MARGIN + owner.detail_height(self.owner_device_count(owner)),
        ))
    }

    /// The downward shift applied to an element whose natural (un-expanded) top is `natural_y`:
    /// the block height for anything at or below the owner's row bottom, else zero.
    fn shift_below(self, natural_y: f64) -> f64 {
        match self.detail_block() {
            Some((insert_y, block_h)) if natural_y >= insert_y => block_h,
            _ => 0.,
        }
    }
}

/// The gsettings-backed tiles, in grid order (row-major, two columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tile {
    DarkStyle,
    DoNotDisturb,
    NightLight,
}

impl Tile {
    fn label(self) -> &'static str {
        match self {
            Tile::DarkStyle => "Dark Style",
            Tile::DoNotDisturb => "Do Not Disturb",
            Tile::NightLight => "Night Light",
        }
    }

    /// Candidate symbolic icon names, first that resolves wins.
    fn icons(self) -> &'static [&'static str] {
        match self {
            Tile::DarkStyle => &["dark-mode-symbolic", "weather-clear-night-symbolic"],
            Tile::DoNotDisturb => &["notifications-disabled-symbolic", "notification-symbolic"],
            Tile::NightLight => &["night-light-symbolic", "display-brightness-symbolic"],
        }
    }

    fn is_on(self, t: QuickToggles) -> bool {
        match self {
            Tile::DarkStyle => t.dark_style,
            Tile::DoNotDisturb => t.do_not_disturb,
            Tile::NightLight => t.night_light,
        }
    }

    /// The action that sets this tile to `on`.
    fn action(self, on: bool) -> PopoverAction {
        match self {
            Tile::DarkStyle => PopoverAction::SetDarkStyle(on),
            Tile::DoNotDisturb => PopoverAction::SetDoNotDisturb(on),
            Tile::NightLight => PopoverAction::SetNightLight(on),
        }
    }
}

/// A cell in the quick-settings grid: either a gsettings-backed [`Tile`] toggle or
/// the live **Network** status tile — whose state comes from the NetworkManager
/// watcher ([`crate::system_status`]) rather than gsettings, and whose click opens
/// network settings rather than flipping a key (the in-menu enable/disable toggle and
/// the connection sub-menu are deferred, like gnome-shell's `NMDeviceToggle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridTile {
    Network,
    Toggle(Tile),
    /// Power Mode (power-profiles-daemon) — a live D-Bus-backed toggle, shown only when the daemon
    /// is present. Its label carries a second (subtitle) line with the active profile name, its
    /// icon tracks the profile, its body-click toggles Balanced ↔ last-selected, and its arrow
    /// opens the profile picker (gnome-shell's `PowerProfilesToggle`, a `QuickMenuToggle`).
    PowerProfile,
    /// Airplane (rfkill) mode — a live D-Bus-backed toggle (gsd-rfkill), shown only when the
    /// hardware has rfkill switches (`ShouldShowAirplaneMode`). Unlike [`Toggle`](Self::Toggle)
    /// its state isn't gsettings; unlike [`Network`](Self::Network) its click flips a value.
    Airplane,
}

/// The always-present grid tiles, row-major over two columns: Network leads in the prominent
/// top-left cell (gnome-shell's quick-settings grid leads with connectivity), then the gsettings
/// toggles fill out the 2×2. The PowerProfile and Airplane tiles are *appended* to this when shown
/// (see [`grid`]) — gnome-shell adds both after every tile we carry (`panel.js`
/// `QUICK_SETTINGS_ITEMS` order: powerProfiles before rfkill, which our append order preserves).
const BASE_GRID: [GridTile; 4] = [
    GridTile::Network,
    GridTile::Toggle(Tile::DarkStyle),
    GridTile::Toggle(Tile::DoNotDisturb),
    GridTile::Toggle(Tile::NightLight),
];

/// The live grid: [`BASE_GRID`] plus the two conditional tiles. **They are always appended in this
/// exact order — PowerProfile then Airplane — never inserted.** `network_index` and
/// `power_profile_index`/`anchor_row_bottom` resolve tile identity by a *constant* index over
/// `BASE_GRID` (Network at its `BASE_GRID` slot; PowerProfile, the first conditional, always at
/// `BASE_GRID.len()`), which holds only while PowerProfile precedes Airplane. Two debug_asserts at
/// the hit site (`pointer_click`) pin both the prefix and the append order.
fn grid(show_power_profile: bool, show_airplane: bool) -> Vec<GridTile> {
    let mut tiles = BASE_GRID.to_vec();
    if show_power_profile {
        tiles.push(GridTile::PowerProfile);
    }
    if show_airplane {
        tiles.push(GridTile::Airplane);
    }
    tiles
}

impl GridTile {
    /// The tile's (title) label given the live toggle + network state. Power Mode's title is
    /// static; its active profile is the [`subtitle`](Self::subtitle) line.
    fn label(self, network: NetworkStatus) -> String {
        match self {
            GridTile::Toggle(t) => t.label().to_string(),
            GridTile::Network => network_label(network).to_string(),
            GridTile::PowerProfile => "Power Mode".to_string(),
            GridTile::Airplane => "Airplane Mode".to_string(),
        }
    }

    /// The tile's second (subtitle) line, or `None` for a single-line tile. Only Power Mode has one
    /// (the active profile name), mirroring gnome-shell's `QuickMenuToggle` subtitle.
    fn subtitle(self, power: &PowerProfileStatus) -> Option<String> {
        match self {
            GridTile::PowerProfile => Some(power.name().to_string()),
            _ => None,
        }
    }

    /// Candidate symbolic icon names, first that resolves wins.
    fn icons(self, network: NetworkStatus, power: &PowerProfileStatus) -> Vec<String> {
        match self {
            GridTile::Toggle(t) => t.icons().iter().map(|s| s.to_string()).collect(),
            GridTile::Network => network_icons(network),
            GridTile::PowerProfile => vec![power.icon().to_string()],
            GridTile::Airplane => vec!["airplane-mode-symbolic".to_string()],
        }
    }

    /// Whether the tile reads as "on" (accent background): a toggle's gsettings state, Network's
    /// connected state, Power Mode's non-Balanced state, or Airplane's active state.
    fn is_on(
        self,
        toggles: QuickToggles,
        network: NetworkStatus,
        airplane: AirplaneStatus,
        power: &PowerProfileStatus,
    ) -> bool {
        match self {
            GridTile::Toggle(t) => t.is_on(toggles),
            GridTile::Network => {
                matches!(network, NetworkStatus::Wired | NetworkStatus::Wireless(_))
            }
            GridTile::PowerProfile => power.is_active(),
            GridTile::Airplane => airplane.active,
        }
    }

    /// Whether this tile carries an expand-arrow that opens a detail view (gnome-shell's
    /// `QuickMenuToggle`): Network and Power Mode. The toggles/Airplane are plain [`QuickToggle`]s.
    /// (Power Mode's arrow is additionally gated on >2 profiles in [`tile_arrow_rect`],
    /// gnome-shell's `menuEnabled`.)
    fn detail_owner(self) -> Option<DetailOwner> {
        match self {
            GridTile::Network => Some(DetailOwner::Network),
            GridTile::PowerProfile => Some(DetailOwner::PowerProfile),
            GridTile::Toggle(_) | GridTile::Airplane => None,
        }
    }
}

/// The Network tile's label. The status model carries no SSID / connection name yet,
/// so these are gnome-shell's generic per-type fallbacks.
fn network_label(network: NetworkStatus) -> &'static str {
    match network {
        NetworkStatus::Wired => "Wired",
        NetworkStatus::Wireless(_) => "Wi-Fi",
        NetworkStatus::Offline => "Offline",
        NetworkStatus::Unknown => "Network",
    }
}

/// The Network tile's icon candidates, falling back to a wired glyph while the state
/// is `Unknown` (pre-first-read / no `dbus` feature) so the tile always shows an icon.
fn network_icons(network: NetworkStatus) -> Vec<String> {
    system_status::network_icon(network)
        .map(|c| c.iter().map(|s| s.to_string()).collect())
        .unwrap_or_else(|| vec!["network-wired-symbolic".to_string()])
}

/// The system-row buttons (gnome-shell's `SystemItem`, `js/ui/status/system.js`):
/// screenshot + settings, then lock + power.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysButton {
    Screenshot,
    Settings,
    Lock,
    Power,
}

const SYS_BUTTONS: [SysButton; 4] = [
    SysButton::Screenshot,
    SysButton::Settings,
    SysButton::Lock,
    SysButton::Power,
];

impl SysButton {
    fn icons(self) -> &'static [&'static str] {
        match self {
            // gnome-shell uses `screenshooter-symbolic` (its own bundled icon),
            // absent from Adwaita on disk — fall back to what the theme ships.
            SysButton::Screenshot => &[
                "screenshooter-symbolic",
                "applets-screenshooter-symbolic",
                "camera-photo-symbolic",
            ],
            SysButton::Settings => &[
                "org.gnome.Settings-symbolic",
                "emblem-system-symbolic",
                "applications-system-symbolic",
            ],
            SysButton::Lock => &["system-lock-screen-symbolic", "changes-prevent-symbolic"],
            SysButton::Power => &["system-shutdown-symbolic"],
        }
    }

    /// The detail view this button opens instead of acting immediately, if any. gnome-shell's
    /// power button is a `hasMenu` `ShutdownItem`: it opens the session submenu rather than
    /// powering off directly (which is what our button used to do).
    fn detail_owner(self) -> Option<DetailOwner> {
        match self {
            SysButton::Power => Some(DetailOwner::Power),
            _ => None,
        }
    }

    /// What clicking this button does (for the buttons that act immediately). Screenshot opens the
    /// interactive UI (like gnome-shell's `Main.screenshotUI.open`); Settings opens control-center;
    /// Lock asks logind to lock. Power has a [`detail_owner`](Self::detail_owner) and must be
    /// routed through it (the session submenu), never here — calling `action()` on it would
    /// reintroduce the one-click-no-confirm power-off this menu deliberately removed.
    fn action(self) -> PopoverAction {
        let words: &[&str] = match self {
            SysButton::Screenshot => return PopoverAction::Screenshot,
            SysButton::Settings => &["gnome-control-center"],
            SysButton::Lock => &["loginctl", "lock-session"],
            SysButton::Power => {
                unreachable!("the power button opens its submenu via detail_owner(), not action()")
            }
        };
        PopoverAction::Spawn(words.iter().map(|s| s.to_string()).collect())
    }
}

/// The quick-settings menu. Holds its own copy of the toggle states so a click
/// updates the tile immediately (the write-back round-trips through gsettings).
pub struct QuickSettings {
    toggles: QuickToggles,
    /// Live network state (from the system-bus watcher), for the Network grid tile.
    network: NetworkStatus,
    /// Airplane (rfkill) state, for the conditionally-shown Airplane grid tile. `show` grows the
    /// grid by one tile; `active` is the tile's on-state.
    airplane: AirplaneStatus,
    /// Power-profile state, for the conditionally-shown Power Mode grid tile. `show` grows the
    /// grid; `active`/profiles drive its subtitle, icon, on-state, and (next slice) its
    /// picker.
    power: PowerProfileStatus,
    /// The battery, for the far-left power pill; `None` hides it (desktop / VM
    /// without a battery), like gnome-shell's `PowerToggle.visible = IsPresent`.
    battery: Option<BatteryStatus>,
    /// Accent color for an active tile's background (straight RGBA).
    accent: [f32; 4],
    /// Default-sink state for the volume slider; `None` hides the slider row.
    audio: Option<AudioStatus>,
    /// The output sinks + current default, for the output slider's device picker (empty → no
    /// picker arrow).
    sink_list: SinkList,
    /// Microphone state (level/mute + recording/source-present visibility) for the mic slider.
    mic: MicStatus,
    /// The input sources + current default, for the mic slider's device picker.
    source_list: SourceList,
    /// The slider currently being dragged (a button held on its track) and the device count frozen
    /// at drag start — so a device hot-plug mid-drag can't add/remove that slider's picker arrow,
    /// resize the track, and remap `volume_from_x` (snapping the level under a stationary
    /// pointer). `None` when not dragging; [`layout`](Self::layout) threads it into
    /// `Layout::drag`.
    sliding: Option<(Slider, usize)>,
    /// Which tile's detail view is open (gnome-shell's single open `QuickToggleMenu`), or `None`
    /// when collapsed. At most one at a time.
    expanded: Option<DetailOwner>,
    /// The control the pointer is hovering, highlighted on render.
    hovered: Option<QsHover>,
    /// Bumped on any toggle so the cached chrome texture is redrawn.
    revision: u64,
    cache: RefCell<TextureCache>,
}

/// A hoverable control in the quick-settings menu, for the hover highlight. A
/// menu tile lights as a whole (its body and picker-arrow halves together — a
/// small divergence from gnome-shell's per-half hover) and the slider picker
/// arrow is not separately highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QsHover {
    Tile(usize),
    Pill,
    Sys(SysButton),
    SliderIcon(Slider),
    DetailRow(usize),
}

struct TextureCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl QuickSettings {
    /// Open with the current toggle states, network state, and battery; `accent`
    /// straight RGB (e.g. `gnome_settings.accent_color`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        toggles: QuickToggles,
        network: NetworkStatus,
        airplane: AirplaneStatus,
        power: PowerProfileStatus,
        battery: Option<BatteryStatus>,
        audio: Option<AudioStatus>,
        sink_list: SinkList,
        mic: MicStatus,
        source_list: SourceList,
        accent: [u8; 3],
    ) -> Self {
        Self {
            toggles,
            network,
            airplane,
            power,
            battery,
            audio,
            sink_list,
            mic,
            source_list,
            sliding: None,
            expanded: None,
            hovered: None,
            accent: [
                f32::from(accent[0]) / 255.,
                f32::from(accent[1]) / 255.,
                f32::from(accent[2]) / 255.,
                1.,
            ],
            revision: 0,
            cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
        }
    }

    /// Whether the battery pill is shown (governs the system-row layout).
    fn has_pill(&self) -> bool {
        self.battery.is_some()
    }

    /// Which slider rows are present: the output slider whenever a sink is bound; the mic slider
    /// only while recording with a bound source (gnome-shell's `_shouldBeVisible`,
    /// `volume.js:429`).
    fn sliders(&self) -> Sliders {
        Sliders {
            output: self.audio.is_some(),
            mic: self.mic.recording && self.mic.source_present,
        }
    }

    /// The live grid tiles (Network + toggles, plus Power Mode / Airplane when their daemons are
    /// present).
    fn grid(&self) -> Vec<GridTile> {
        grid(self.power.show, self.airplane.show)
    }

    /// The current layout context (slider presence + which detail view is open + device counts +
    /// the active drag + tile count), the single source of truth every geometry function shares.
    fn layout(&self) -> Layout {
        Layout {
            sliders: self.sliders(),
            expanded: self.expanded,
            sink_count: self.sink_list.sinks.len(),
            source_count: self.source_list.sources.len(),
            profile_count: self.power.available.len(),
            drag: self.sliding,
            grid_len: self.grid().len(),
        }
    }

    /// Adopt a fresh airplane-mode snapshot (from the gsd-rfkill watcher). `show` grows/shrinks the
    /// grid (a 5th tile); `active` flips the tile. Returns whether it changed.
    pub fn set_airplane(&mut self, airplane: AirplaneStatus) -> bool {
        if self.airplane == airplane {
            return false;
        }
        self.airplane = airplane;
        self.revision += 1;
        true
    }

    /// Adopt a fresh power-profile snapshot (from power-profiles-daemon). `show` grows/shrinks the
    /// grid by the Power Mode tile; `active`/`available` drive its subtitle, icon, on-state, and
    /// the picker rows. Returns whether it changed. Calls `normalize_expanded` (unlike
    /// `set_airplane`), because an open picker must collapse if the daemon vanishes or drops to
    /// ≤2 profiles — its arrow gate — or the card would pin below a vanished/arrow-less tile.
    pub fn set_power_profile(&mut self, power: PowerProfileStatus) -> bool {
        if self.power == power {
            return false;
        }
        self.power = power;
        self.revision += 1;
        self.normalize_expanded();
        true
    }

    /// Enforce the invariant that an open detail view's owner still exists: the Output picker is
    /// valid only while there's a slider (a bound sink) AND more than one sink to choose between —
    /// the same `>1` gate that shows its arrow. If a sink is removed (down to one) or the default
    /// unbinds while the picker is open, collapse it, so the card can't pin below a vanished slider
    /// row (broken geometry) or strand the user with an open card and no arrow to close it. Returns
    /// whether it collapsed (→ redraw). Network/Power owners always exist, so they're untouched.
    fn normalize_expanded(&mut self) -> bool {
        let sliders = self.sliders();
        let invalid = match self.expanded {
            // Output valid only while its slider exists AND >1 sink (the arrow's gate).
            Some(DetailOwner::Output) => !sliders.output || self.sink_list.sinks.len() <= 1,
            // Input the same, off the mic slider + source count.
            Some(DetailOwner::Input) => !sliders.mic || self.source_list.sources.len() <= 1,
            // Power picker valid only while the daemon is present AND >2 profiles (its arrow gate).
            Some(DetailOwner::PowerProfile) => !self.power.show || self.power.available.len() <= 2,
            _ => false,
        };
        if invalid {
            self.expanded = None;
            self.revision += 1;
            return true;
        }
        false
    }

    /// Adopt a fresh output-sink list (from the PipeWire watcher). Returns whether it changed.
    pub fn set_sink_list(&mut self, sink_list: SinkList) -> bool {
        let mut changed = self.sink_list != sink_list;
        if changed {
            self.sink_list = sink_list;
            self.revision += 1;
        }
        changed |= self.normalize_expanded();
        changed
    }

    /// The menu's logical size: two tile columns + the system row, grown by the open detail view.
    pub fn logical_size(&self) -> Size<f64, Logical> {
        Size::from((menu_w(), menu_h(self.layout())))
    }

    /// Handle a click at a menu-local logical position, returning the action to
    /// apply (or [`PopoverAction::Consumed`] for a click that hit nothing
    /// actionable but is still inside the menu). A tile click also flips the
    /// tile's own state so it updates before the gsettings write round-trips.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let layout = self.layout();
        // An open detail view is topmost: a row runs its action (a `Spawn`, which closes the
        // menu — so no need to collapse first); a click elsewhere in the card is swallowed.
        if let Some(owner) = self.expanded {
            for (k, row) in owner
                .rows(
                    self.network,
                    &self.sink_list,
                    &self.source_list,
                    &self.power,
                )
                .into_iter()
                .enumerate()
            {
                if detail_row_rect(k, layout).is_some_and(|r| r.contains(pos)) {
                    return row.action;
                }
            }
            if detail_rect(layout).is_some_and(|r| r.contains(pos)) {
                return PopoverAction::Consumed;
            }
        }
        let tiles = self.grid();
        debug_assert_eq!(
            &tiles[..BASE_GRID.len()],
            &BASE_GRID,
            "grid() must APPEND the conditional tiles, never insert (network_index depends on it)"
        );
        debug_assert!(
            tiles
                .iter()
                .position(|t| matches!(t, GridTile::PowerProfile))
                .is_none_or(|i| i == power_profile_index()),
            "PowerProfile must be the FIRST appended tile (before Airplane) — \
             power_profile_index()/anchor_row_bottom assume its constant index"
        );
        for (i, &item) in tiles.iter().enumerate() {
            // A menu tile's arrow-half toggles its detail view (open, or close if already open —
            // one at a time); the toggle-body keeps the tile's own behavior. A plain tile is all
            // body.
            if tile_arrow_rect(i, item, layout).is_some_and(|r| r.contains(pos)) {
                let owner = item.detail_owner();
                self.expanded = if self.expanded == owner { None } else { owner };
                self.revision += 1;
                return PopoverAction::Consumed;
            }
            if tile_body_rect(i, item, layout).contains(pos) {
                return match item {
                    // Network body: open settings (the in-place enable/disable toggle is deferred);
                    // the arrow opens the detail view.
                    GridTile::Network => PopoverAction::Spawn(
                        ["gnome-control-center", "network"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    ),
                    GridTile::Toggle(tile) => {
                        let on = !tile.is_on(self.toggles);
                        self.set_tile(tile, on);
                        self.revision += 1;
                        tile.action(on)
                    }
                    // Airplane: a D-Bus write (not optimistic — the tile updates on the gsd echo,
                    // like `SetDefaultSink`; a rejected/hw-blocked write has no corrective echo).
                    // We target the negation of the last *echoed* state, so a rapid second click
                    // before the echo re-sends the same value rather than toggling back
                    // (gnome-shell reads its freshly-written cache and would
                    // toggle back) — an accepted minor divergence for the
                    // sub-round-trip double-click window.
                    GridTile::Airplane => PopoverAction::SetAirplaneMode(!self.airplane.active),
                    // Power Mode body: gnome-shell's `clicked` — toggle Balanced ↔ last-selected.
                    // Which target that is depends on state the compositor owns (the last-selected
                    // gsettings/memory), so we defer the choice to `apply_popover_action`. Also
                    // echo-driven (no local flip).
                    GridTile::PowerProfile => PopoverAction::TogglePowerProfile,
                };
            }
        }
        // The battery pill opens power settings (gnome-shell's PowerToggle).
        if let Some(pill) = pill_rect(self.has_pill()) {
            if pill.contains(pos) {
                return PopoverAction::Spawn(
                    ["gnome-control-center", "power"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                );
            }
        }
        for button in SYS_BUTTONS {
            if sys_rect(button, self.has_pill()).contains(pos) {
                // The power button opens its session submenu (toggle, one detail at a time)
                // instead of acting; the others run their action immediately.
                if let Some(owner) = button.detail_owner() {
                    self.expanded = if self.expanded == Some(owner) {
                        None
                    } else {
                        Some(owner)
                    };
                    self.revision += 1;
                    return PopoverAction::Consumed;
                }
                return button.action();
            }
        }
        // The volume sliders (output, then mic): the icon toggles mute; the arrow (when >1 device)
        // toggles the device picker; the track jumps to (and begins dragging toward) the clicked
        // position. The arrow is tested BEFORE the track so an arrow click never starts a drag (the
        // track is genuinely shortened to make room, so `volume_from_x` stays in range).
        for slider in [Slider::Output, Slider::Mic] {
            if !layout.sliders.present(slider) {
                continue;
            }
            if slider_icon_rect(slider, layout).contains(pos) {
                return match slider {
                    Slider::Output => PopoverAction::ToggleMute,
                    Slider::Mic => PopoverAction::ToggleInputMute,
                };
            }
            if slider_arrow_rect(slider, layout).is_some_and(|r| r.contains(pos)) {
                let owner = slider.owner();
                self.expanded = if self.expanded == Some(owner) {
                    None
                } else {
                    Some(owner)
                };
                self.revision += 1;
                return PopoverAction::Consumed;
            }
            if slider_track_rect(slider, layout).contains(pos) {
                self.sliding = Some((slider, self.device_count(slider)));
                return self.set_local_volume(slider, volume_from_x(slider, pos.x, layout));
            }
        }
        PopoverAction::Consumed
    }

    /// The live device count backing a slider's picker (sinks for Output, sources for Mic) — the
    /// value frozen at drag start.
    fn device_count(&self, slider: Slider) -> usize {
        match slider {
            Slider::Output => self.sink_list.sinks.len(),
            Slider::Mic => self.source_list.sources.len(),
        }
    }

    /// Continue a slider drag: while a button is held on the track, motion updates
    /// the volume. Returns the action to apply, or `None` when not dragging.
    pub fn pointer_drag(&mut self, pos: Point<f64, Logical>) -> Option<PopoverAction> {
        let layout = self.layout();
        let (slider, _) = self.sliding?;
        Some(self.set_local_volume(slider, volume_from_x(slider, pos.x, layout)))
    }

    /// Update the hovered control from a menu-local position (`None` clears the
    /// hover). Returns whether it changed, so the caller can redraw; a change
    /// bumps `revision`, re-baking the chrome with the highlight.
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let new = pos.and_then(|pos| self.hover_zone(pos));
        if new == self.hovered {
            return false;
        }
        self.hovered = new;
        self.revision += 1;
        true
    }

    /// The hoverable control at menu-local `pos`, mirroring the hit order of
    /// [`pointer_click`](Self::pointer_click) but only for controls that carry a
    /// visible highlight (the whole tile, the pill, a system button, a slider
    /// mute icon, or a detail-view row).
    fn hover_zone(&self, pos: Point<f64, Logical>) -> Option<QsHover> {
        let layout = self.layout();
        // An open detail view is topmost: a row highlights; the rest of the card
        // swallows the hover (no tile leaks through).
        if let Some(owner) = self.expanded {
            let rows = owner.rows(
                self.network,
                &self.sink_list,
                &self.source_list,
                &self.power,
            );
            for k in 0..rows.len() {
                if detail_row_rect(k, layout).is_some_and(|r| r.contains(pos)) {
                    return Some(QsHover::DetailRow(k));
                }
            }
            if detail_rect(layout).is_some_and(|r| r.contains(pos)) {
                return None;
            }
        }
        for (i, _item) in self.grid().iter().enumerate() {
            if tile_rect(i, layout).contains(pos) {
                return Some(QsHover::Tile(i));
            }
        }
        if pill_rect(self.has_pill()).is_some_and(|p| p.contains(pos)) {
            return Some(QsHover::Pill);
        }
        for button in SYS_BUTTONS {
            if sys_rect(button, self.has_pill()).contains(pos) {
                return Some(QsHover::Sys(button));
            }
        }
        for slider in [Slider::Output, Slider::Mic] {
            if layout.sliders.present(slider) && slider_icon_rect(slider, layout).contains(pos) {
                return Some(QsHover::SliderIcon(slider));
            }
        }
        None
    }

    /// End a slider drag (pointer released). Returns whether releasing the frozen device count
    /// changed the dragged slider's arrow geometry (a device hot-plugged mid-drag) — the caller
    /// redraws so the picker arrow that was suppressed during the drag now appears.
    pub fn end_drag(&mut self) -> bool {
        let Some((slider, frozen)) = self.sliding.take() else {
            return false;
        };
        frozen != self.device_count(slider)
    }

    /// Optimistically move a slider locally (so the handle tracks the pointer before the PipeWire
    /// echo lands) and emit the matching volume action.
    fn set_local_volume(&mut self, slider: Slider, volume: f64) -> PopoverAction {
        self.revision += 1;
        match slider {
            Slider::Output => {
                if let Some(audio) = self.audio.as_mut() {
                    audio.volume = volume;
                }
                PopoverAction::SetVolume(volume)
            }
            Slider::Mic => {
                self.mic.volume = volume;
                PopoverAction::SetInputVolume(volume)
            }
        }
    }

    /// Adopt a fresh sink state (from the PipeWire watcher). While dragging the OUTPUT slider the
    /// lagging level echo is ignored (the optimistic drag value wins) — but the sink *vanishing*
    /// (unplug) is always honored and cancels the drag, or the event-driven `None` (never re-sent,
    /// `on_audio_status` dedups) would strand a dead slider forever, mirroring `set_mic`. Returns
    /// whether it changed.
    pub fn set_audio(&mut self, audio: Option<AudioStatus>) -> bool {
        if audio == self.audio {
            return false;
        }
        if matches!(self.sliding, Some((Slider::Output, _))) {
            if audio.is_some() {
                // Still present: keep the optimistic drag value, don't yank.
                return false;
            }
            // The sink vanished under the drag: cancel it before the slider hides.
            self.sliding = None;
        }
        self.audio = audio;
        self.revision += 1;
        // The slider vanishing (no bound sink) must also close an open output picker, or its card
        // pins below a slider row that's no longer there.
        self.normalize_expanded();
        true
    }

    /// Adopt a fresh mic snapshot (from the PipeWire watcher). While the mic slider is being
    /// dragged, the live level/mute is ignored (the optimistic drag value wins, like `set_audio`) —
    /// but a *visibility* change (recording or the source stopping) is always honored:
    /// `publish_mic` dedups, so a dropped `recording:false` would never be re-sent and the
    /// slider (and any open input picker) would linger forever, emitting volume at a gone
    /// source. Returns whether it changed.
    pub fn set_mic(&mut self, mic: MicStatus) -> bool {
        if mic == self.mic {
            return false;
        }
        if matches!(self.sliding, Some((Slider::Mic, _))) {
            if mic.recording && mic.source_present {
                // Still visible: keep the optimistic level/mute, don't move the slider mid-drag.
                return false;
            }
            // The mic slider is vanishing under the drag: cancel it before it hides.
            self.sliding = None;
        }
        self.mic = mic;
        self.revision += 1;
        // The mic slider vanishing must also close an open input picker.
        self.normalize_expanded();
        true
    }

    /// Adopt a fresh input-source list (from the PipeWire watcher). Returns whether it changed.
    pub fn set_source_list(&mut self, source_list: SourceList) -> bool {
        let mut changed = self.source_list != source_list;
        if changed {
            self.source_list = source_list;
            self.revision += 1;
        }
        changed |= self.normalize_expanded();
        changed
    }

    fn set_tile(&mut self, tile: Tile, on: bool) {
        match tile {
            Tile::DarkStyle => self.toggles.dark_style = on,
            Tile::DoNotDisturb => self.toggles.do_not_disturb = on,
            Tile::NightLight => self.toggles.night_light = on,
        }
    }

    /// The composited elements for the menu at `origin` (menu-local → output-local
    /// offset): the chrome texture first (topmost after reversal is handled by the
    /// caller pushing in order), then each resolved icon on top of its slot.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();
        let layout = self.layout();

        // Tile icons (drawn above the chrome, so pushed before it).
        for (i, item) in self.grid().into_iter().enumerate() {
            let on = item.is_on(self.toggles, self.network, self.airplane, &self.power);
            let color = if on { FG_ON } else { FG_OFF };
            let rect = tile_rect(i, layout);
            let center = Point::from((
                rect.loc.x + TILE_ICON_INSET + TILE_ICON / 2.,
                rect.loc.y + rect.size.h / 2.,
            ));
            let candidates = item.icons(self.network, &self.power);
            if let Some(el) = icon_element(
                renderer,
                icons,
                &candidates,
                TILE_ICON,
                scale,
                color,
                origin,
                center,
            ) {
                elements.push(el);
            }

            // A menu tile's expand-arrow, centered in its arrow-half (gnome-shell's static
            // `go-next-symbolic`).
            if let Some(arrow) = tile_arrow_rect(i, item, layout) {
                let center = Point::from((
                    arrow.loc.x + arrow.size.w / 2.,
                    arrow.loc.y + arrow.size.h / 2.,
                ));
                if let Some(el) = icon_element(
                    renderer,
                    icons,
                    ARROW_ICONS,
                    ARROW_ICON,
                    scale,
                    color,
                    origin,
                    center,
                ) {
                    elements.push(el);
                }
            }
        }

        // The open detail view: its header icon and any per-row icons (the card background,
        // title, and row labels are chrome, drawn below).
        if let Some(owner) = self.expanded {
            if let Some(card) = detail_rect(layout) {
                let (cand, _title) = owner.header(self.network);
                let center = Point::from((
                    card.loc.x + DETAIL_HEADER_INSET + DETAIL_HEADER_ICON / 2.,
                    card.loc.y + DETAIL_PAD + DETAIL_HEADER_H / 2.,
                ));
                if let Some(el) = icon_element(
                    renderer,
                    icons,
                    &cand,
                    DETAIL_HEADER_ICON,
                    scale,
                    FG_OFF,
                    origin,
                    center,
                ) {
                    elements.push(el);
                }
                for (k, row) in owner
                    .rows(
                        self.network,
                        &self.sink_list,
                        &self.source_list,
                        &self.power,
                    )
                    .into_iter()
                    .enumerate()
                {
                    let Some(rrect) = detail_row_rect(k, layout) else {
                        continue;
                    };
                    // A leading row icon (none for the current consumers), if any.
                    if !row.icons.is_empty() {
                        let center = Point::from((
                            rrect.loc.x + DETAIL_ROW_INSET + TILE_ICON / 2.,
                            rrect.loc.y + rrect.size.h / 2.,
                        ));
                        if let Some(el) = icon_element(
                            renderer, icons, &row.icons, TILE_ICON, scale, FG_OFF, origin, center,
                        ) {
                            elements.push(el);
                        }
                    }
                    // The trailing check on the selected row (gnome-shell's `Ornament.CHECK`).
                    if row.selected {
                        let center = Point::from((
                            rrect.loc.x + rrect.size.w - DETAIL_ROW_INSET - TILE_ICON / 2.,
                            rrect.loc.y + rrect.size.h / 2.,
                        ));
                        if let Some(el) = icon_element(
                            renderer,
                            icons,
                            CHECK_ICONS,
                            TILE_ICON,
                            scale,
                            FG_OFF,
                            origin,
                            center,
                        ) {
                            elements.push(el);
                        }
                    }
                }
            }
        }

        // System-row icons.
        for button in SYS_BUTTONS {
            let rect = sys_rect(button, self.has_pill());
            let center =
                Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));
            if let Some(el) = icon_element(
                renderer,
                icons,
                button.icons(),
                SYS_ICON,
                scale,
                SYS_FG,
                origin,
                center,
            ) {
                elements.push(el);
            }
        }

        // The battery pill's icon, at its left (the percentage label is chrome).
        if let (Some(battery), Some(pill)) = (&self.battery, pill_rect(self.has_pill())) {
            let center = Point::from((
                pill.loc.x + PILL_ICON_INSET + SYS_ICON / 2.,
                pill.loc.y + pill.size.h / 2.,
            ));
            let candidates = system_status::battery_icon(battery);
            if let Some(el) = icon_element(
                renderer,
                icons,
                &candidates,
                SYS_ICON,
                scale,
                SYS_FG,
                origin,
                center,
            ) {
                elements.push(el);
            }
        }

        // Each present slider's mute/level icon (speaker for output, mic for input) in its disc,
        // plus its device-picker arrow at the right (when >1 device).
        for slider in [Slider::Output, Slider::Mic] {
            if !layout.sliders.present(slider) {
                continue;
            }
            let disc = slider_icon_rect(slider, layout);
            let center =
                Point::from((disc.loc.x + disc.size.w / 2., disc.loc.y + disc.size.h / 2.));
            let name = match slider {
                Slider::Output => crate::audio::volume_icon(&self.audio.unwrap_or_default()),
                Slider::Mic => crate::audio::mic_volume_icon(&self.mic),
            }
            .to_string();
            if let Some(el) = icon_element(
                renderer,
                icons,
                &[name],
                SLIDER_ICON,
                scale,
                SYS_FG,
                origin,
                center,
            ) {
                elements.push(el);
            }
            if let Some(arrow) = slider_arrow_rect(slider, layout) {
                let center = Point::from((
                    arrow.loc.x + arrow.size.w / 2.,
                    arrow.loc.y + arrow.size.h / 2.,
                ));
                if let Some(el) = icon_element(
                    renderer,
                    icons,
                    ARROW_ICONS,
                    ARROW_ICON,
                    scale,
                    SYS_FG,
                    origin,
                    center,
                ) {
                    elements.push(el);
                }
            }
        }

        // The chrome (menu + tile backgrounds + labels), beneath the icons.
        match self.texture(renderer, scale) {
            Ok(texture) => {
                // The menu's outer corners are rounded (transparent), so report opacity as the two
                // bands that exclude the four `MENU_RADIUS` corner squares — never claiming a
                // cut-away corner pixel is opaque (which would let occlusion drop what shows
                // through). Under-reporting the small arc/square slivers is harmless.
                let size = texture.size();
                let r = (MENU_RADIUS * scale).round() as i32;
                let opaque = if r > 0 && size.w > 2 * r && size.h > 2 * r {
                    vec![
                        Rectangle::new(
                            Point::<i32, BufferCoord>::from((0, r)),
                            Size::from((size.w, size.h - 2 * r)),
                        ),
                        Rectangle::new(
                            Point::<i32, BufferCoord>::from((r, 0)),
                            Size::from((size.w - 2 * r, size.h)),
                        ),
                    ]
                } else {
                    vec![Rectangle::from_size(size)]
                };
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    opaque,
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error drawing the quick-settings menu: {err:#}"),
        }

        elements
    }

    /// Draw (or reuse) the chrome texture, caching per (scale, revision).
    fn texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.textures.clear();
            cache.context = Some(context);
        }
        let fresh =
            matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == self.revision);
        if !fresh {
            let tex = self.draw(renderer, scale)?;
            cache.textures.insert(scale_key, (self.revision, tex));
        }
        Ok(cache
            .textures
            .get(&scale_key)
            .map(|(_, t)| t.clone())
            .unwrap())
    }

    fn draw(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let _span = tracy_client::span!("quick_settings::draw");

        let layout = self.layout();
        let size = self.logical_size();
        let w_px = to_physical_precise_round::<i32>(scale, size.w).max(1);
        let h_px = to_physical_precise_round::<i32>(scale, size.h).max(1);
        let phys = Size::<i32, Physical>::from((w_px, h_px));
        let label_px = (LABEL_PX * scale) as f32;

        // Shape the tile labels + the battery pill's percentage up front (immutable
        // borrows of the font system).
        let labels: Vec<String> = self
            .grid()
            .iter()
            .map(|item| item.label(self.network))
            .collect();
        // `.quick-toggle-title` / the power toggle's percentage are %heading = weight 700.
        let label_runs: Vec<_> = labels
            .iter()
            .map(|l| renderer.build_glyph_run_weighted(l, label_px, true))
            .collect::<Result<_, _>>()?;
        // A parallel Option<run> for the (regular-weight) subtitle line — `Some` only for the tiles
        // that carry one (Power Mode's active profile), `None` otherwise.
        let subtitle_px = (SUBTITLE_PX * scale) as f32;
        let subtitle_runs: Vec<Option<_>> = self
            .grid()
            .iter()
            .map(|item| {
                item.subtitle(&self.power)
                    .map(|s| renderer.build_glyph_run_weighted(&s, subtitle_px, false))
                    .transpose()
            })
            .collect::<Result<_, _>>()?;
        let pill_run = self
            .battery
            .as_ref()
            .map(|b| {
                renderer.build_glyph_run_weighted(
                    &format!("{}%", b.percentage.round() as i64),
                    label_px,
                    true,
                )
            })
            .transpose()?;
        // The open detail view's header title (bold, `%title_3`) and its regular-weight row labels.
        let detail_title_px = (DETAIL_TITLE_PX * scale) as f32;
        let detail_row_px = (DETAIL_ROW_PX * scale) as f32;
        let detail_runs = self
            .expanded
            .map(|owner| -> anyhow::Result<_> {
                let (_, title) = owner.header(self.network);
                let title_run = renderer.build_glyph_run_weighted(&title, detail_title_px, true)?;
                let rows = owner.rows(
                    self.network,
                    &self.sink_list,
                    &self.source_list,
                    &self.power,
                );
                // The card is sized from the pure `row_shape`; assert the live rows match it (count
                // + separator positions) so the geometry can't drift from what's drawn here.
                debug_assert_eq!(
                    rows.iter().map(|r| r.separator_before).collect::<Vec<_>>(),
                    owner.row_shape(self.layout().owner_device_count(owner)),
                    "rows() must match row_shape() for correct card sizing"
                );
                let row_runs = rows
                    .into_iter()
                    .map(|r| renderer.build_glyph_run_weighted(&r.label, detail_row_px, false))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((title_run, row_runs))
            })
            .transpose()?;

        let mut target = renderer.create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, BufferCoord>::from((w_px, h_px)),
        )?;
        {
            let mut fb = renderer.bind(&mut target)?;
            let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
            let full = Rectangle::from_size(phys);
            // Rounded panel: clear transparent, then fill the menu background as a rounded rect so
            // the outer corners stay transparent (the composited element's opacity hint below
            // excludes those corners). Content is drawn on top of the opaque interior.
            frame.clear(Color32F::from(TRANSPARENT), &[full])?;
            frame.render_rounded_rect(MENU_BG, (MENU_RADIUS * scale) as f32, full, &[full])?;

            let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
            let rect_px = |r: Rectangle<f64, Logical>| {
                Rectangle::new(
                    Point::<i32, Physical>::from((px(r.loc.x), px(r.loc.y))),
                    Size::<i32, Physical>::from((px(r.size.w), px(r.size.h))),
                )
            };
            // Left-align a shaped run's ink at a physical point, vertically centered.
            let place_left = |ink: (i32, i32, i32, i32), lx: i32, cy: i32| {
                let (ix, iy, _iw, ih) = ink;
                Point::<i32, Physical>::from((lx - ix, cy - ih / 2 - iy))
            };

            for (i, item) in self.grid().into_iter().enumerate() {
                let rect = tile_rect(i, layout);
                let on = item.is_on(self.toggles, self.network, self.airplane, &self.power);
                let bg = if on { self.accent } else { TILE_OFF };
                // gnome-shell quick toggles use `$forced_circular_radius` → pill-shaped; a
                // half-height radius clamps to the pill in `sdf_rect.frag`. Drawn over the opaque
                // MENU_BG, so the cut corners reveal the menu, keeping the texture opaque.
                frame.render_rounded_rect(
                    bg,
                    (rect.size.h / 2. * scale) as f32,
                    rect_px(rect),
                    &[full],
                )?;
                if self.hovered == Some(QsHover::Tile(i)) {
                    frame.render_rounded_rect(
                        HOVER_WASH,
                        (rect.size.h / 2. * scale) as f32,
                        rect_px(rect),
                        &[full],
                    )?;
                }

                // A menu tile's arrow-half is separated from the body by a 1px divider
                // (`.quick-toggle-separator`); v1 keeps one pill background and marks the split
                // with the divider + the arrow icon (the split-radius look is a later cosmetic).
                if let Some(arrow) = tile_arrow_rect(i, item, layout) {
                    let sep = Rectangle::new(
                        Point::from((arrow.loc.x - SEPARATOR_W, arrow.loc.y + arrow.size.h * 0.2)),
                        Size::from((SEPARATOR_W, arrow.size.h * 0.6)),
                    );
                    frame.render_rounded_rect(SEPARATOR_COLOR, 0., rect_px(sep), &[full])?;
                }

                let fg = if on { FG_ON } else { FG_OFF };
                let label_x = px(rect.loc.x + TILE_ICON_INSET + TILE_ICON + 8.);
                let center_y = rect.loc.y + rect.size.h / 2.;
                let run = &label_runs[i];
                // Clip the label to the toggle-body so a long name can't run under the arrow
                // (gnome-shell ellipsizes; clipping is the minimal faithful bound).
                let clip = rect_px(tile_body_rect(i, item, layout));
                match &subtitle_runs[i] {
                    // Two-line tile (Power Mode): title above center, subtitle below.
                    Some(sub) => {
                        frame.render_glyphs(
                            run,
                            place_left(run.ink_bounds(), label_x, px(center_y - SUBTITLE_GAP)),
                            fg,
                            clip,
                            &[full],
                        )?;
                        frame.render_glyphs(
                            sub,
                            place_left(sub.ink_bounds(), label_x, px(center_y + SUBTITLE_GAP)),
                            fg,
                            clip,
                            &[full],
                        )?;
                    }
                    // Single-line tile: the title, vertically centered.
                    None => {
                        frame.render_glyphs(
                            run,
                            place_left(run.ink_bounds(), label_x, px(center_y)),
                            fg,
                            clip,
                            &[full],
                        )?;
                    }
                }
            }

            // The battery pill: a filled slab (its icon composites on top) with the
            // percentage after it.
            if let (Some(pill), Some(run)) = (pill_rect(self.has_pill()), &pill_run) {
                frame.render_rounded_rect(
                    TILE_OFF,
                    (pill.size.h / 2. * scale) as f32,
                    rect_px(pill),
                    &[full],
                )?;
                if self.hovered == Some(QsHover::Pill) {
                    frame.render_rounded_rect(
                        HOVER_WASH,
                        (pill.size.h / 2. * scale) as f32,
                        rect_px(pill),
                        &[full],
                    )?;
                }
                let label_x = px(pill.loc.x + PILL_ICON_INSET + SYS_ICON + 8.);
                let label_cy = px(pill.loc.y + pill.size.h / 2.);
                frame.render_glyphs(
                    run,
                    place_left(run.ink_bounds(), label_x, label_cy),
                    FG_OFF,
                    full,
                    &[full],
                )?;
            }

            // The system-row action buttons (screenshot/settings/lock/power) are
            // gnome-shell `.icon-button`s: a circular button-background disc beneath each
            // symbolic icon (`_buttons.scss`: `@extend %button` + `border-radius:
            // $forced_circular_radius`). SYS_HIT (40px) is the 16px icon plus the button's
            // 12px padding on each side — i.e. the button diameter. The icons composite on
            // top afterwards, like the battery pill above.
            for button in SYS_BUTTONS {
                let hit = sys_rect(button, self.has_pill());
                let disc = Rectangle::new(
                    Point::from((hit.loc.x, hit.loc.y + (hit.size.h - SYS_HIT) / 2.)),
                    Size::from((SYS_HIT, SYS_HIT)),
                );
                frame.render_rounded_rect(
                    TILE_OFF,
                    (SYS_HIT / 2. * scale) as f32,
                    rect_px(disc),
                    &[full],
                )?;
                if self.hovered == Some(QsHover::Sys(button)) {
                    frame.render_rounded_rect(
                        HOVER_WASH,
                        (SYS_HIT / 2. * scale) as f32,
                        rect_px(disc),
                        &[full],
                    )?;
                }
            }

            // Each present slider: mute-button disc, the trough, its accent-filled portion, and the
            // round handle (`.quick-slider` + `_slider.scss`). The level icon composites on top
            // afterwards. Output and mic share the geometry; only the level/mute source differs.
            for slider in [Slider::Output, Slider::Mic] {
                if !layout.sliders.present(slider) {
                    continue;
                }
                let (volume, muted) = match slider {
                    Slider::Output => {
                        let a = self.audio.unwrap_or_default();
                        (a.volume, a.muted)
                    }
                    Slider::Mic => (self.mic.volume, self.mic.muted),
                };
                let disc = slider_icon_rect(slider, layout);
                frame.render_rounded_rect(
                    TILE_OFF,
                    (SLIDER_H / 2. * scale) as f32,
                    rect_px(disc),
                    &[full],
                )?;
                if self.hovered == Some(QsHover::SliderIcon(slider)) {
                    frame.render_rounded_rect(
                        HOVER_WASH,
                        (SLIDER_H / 2. * scale) as f32,
                        rect_px(disc),
                        &[full],
                    )?;
                }

                let track = slider_track_rect(slider, layout);
                let cy = track.loc.y + track.size.h / 2.;
                let trough = Rectangle::new(
                    Point::from((track.loc.x, cy - SLIDER_TROUGH / 2.)),
                    Size::from((track.size.w, SLIDER_TROUGH)),
                );
                frame.render_rounded_rect(
                    SLIDER_TROUGH_BG,
                    (SLIDER_TROUGH / 2. * scale) as f32,
                    rect_px(trough),
                    &[full],
                )?;

                let handle_cx = slider_handle_x(slider, volume, layout);
                let fill_color = if muted { SLIDER_TROUGH_BG } else { self.accent };
                let fill = Rectangle::new(
                    trough.loc,
                    Size::from(((handle_cx - track.loc.x).max(0.), SLIDER_TROUGH)),
                );
                frame.render_rounded_rect(
                    fill_color,
                    (SLIDER_TROUGH / 2. * scale) as f32,
                    rect_px(fill),
                    &[full],
                )?;

                let handle = Rectangle::new(
                    Point::from((handle_cx - SLIDER_HANDLE / 2., cy - SLIDER_HANDLE / 2.)),
                    Size::from((SLIDER_HANDLE, SLIDER_HANDLE)),
                );
                frame.render_rounded_rect(
                    FG_OFF,
                    (SLIDER_HANDLE / 2. * scale) as f32,
                    rect_px(handle),
                    &[full],
                )?;
            }

            // The open detail view: the `%card` background, its header title (the header icon
            // composites on top in `render`), then the row labels. Row icons, if any, also
            // composite on top.
            if let (Some(card), Some((title_run, row_runs))) = (detail_rect(layout), &detail_runs) {
                frame.render_rounded_rect(
                    CARD_BG,
                    (DETAIL_RADIUS * scale) as f32,
                    rect_px(card),
                    &[full],
                )?;

                let title_x = px(card.loc.x + DETAIL_HEADER_INSET + DETAIL_HEADER_ICON + 8.);
                let title_cy = px(card.loc.y + DETAIL_PAD + DETAIL_HEADER_H / 2.);
                frame.render_glyphs(
                    title_run,
                    place_left(title_run.ink_bounds(), title_x, title_cy),
                    FG_OFF,
                    rect_px(card),
                    &[full],
                )?;

                for (k, run) in row_runs.iter().enumerate() {
                    let Some(rrect) = detail_row_rect(k, layout) else {
                        continue;
                    };
                    // A hovered picker row: a faint rounded fill (it has no base
                    // bg) behind the label, matching GNOME's flat menu-item hover.
                    if self.hovered == Some(QsHover::DetailRow(k)) {
                        frame.render_rounded_rect(
                            HOVER_WASH,
                            (8. * scale) as f32,
                            rect_px(rrect),
                            &[full],
                        )?;
                    }
                    let has_icon = self
                        .expanded
                        .map(|o| {
                            o.rows(
                                self.network,
                                &self.sink_list,
                                &self.source_list,
                                &self.power,
                            )
                        })
                        .and_then(|rows| rows.into_iter().nth(k).map(|r| !r.icons.is_empty()))
                        .unwrap_or(false);
                    let label_x = if has_icon {
                        px(rrect.loc.x + DETAIL_ROW_INSET + TILE_ICON + 8.)
                    } else {
                        px(rrect.loc.x + DETAIL_ROW_INSET)
                    };
                    let label_cy = px(rrect.loc.y + rrect.size.h / 2.);
                    // Reserve the trailing check zone (a check icon + its inset) on every row so a
                    // long label (e.g. a verbose HDMI sink description) is clipped before it, not
                    // drawn under the selected row's `object-select-symbolic`.
                    let mut label_clip = rrect;
                    label_clip.size.w =
                        (label_clip.size.w - (DETAIL_ROW_INSET + TILE_ICON)).max(0.);
                    frame.render_glyphs(
                        run,
                        place_left(run.ink_bounds(), label_x, label_cy),
                        FG_OFF,
                        rect_px(label_clip),
                        &[full],
                    )?;
                }
            }

            let _sync = frame.finish()?;
        }

        renderer.make_offscreen_sampleable(&target)?;
        Ok(target)
    }
}

/// The menu's logical width: two tile columns plus padding.
fn menu_w() -> f64 {
    PAD * 2. + COLS as f64 * TILE_W + (COLS as f64 - 1.) * TILE_GAP
}

/// The y of the tile grid's top edge. The grid sits below the system row, and below the volume
/// sliders too when present (gnome-shell orders the output slider then the mic slider right under
/// the system item, above the toggle tiles).
fn grid_top(sliders: Sliders) -> f64 {
    PAD + SYS_H + TILE_GAP + sliders.count() as f64 * (SLIDER_H + TILE_GAP)
}

/// The y of the grid's bottom edge (grid top + tile rows), before any detail shift. Uses the live
/// tile count (`layout.grid_len`) so a 5th (airplane) tile adds a third row.
fn grid_bottom(layout: Layout) -> f64 {
    let rows = layout.grid_len.div_ceil(COLS) as f64;
    grid_top(layout.sliders) + rows * TILE_H + (rows - 1.) * TILE_GAP
}

/// The menu's logical height: system row, sliders, tile grid, padding — grown by the open detail
/// view's block (the card plus its top margin) when one is expanded.
fn menu_h(layout: Layout) -> f64 {
    let base = grid_bottom(layout) + PAD;
    base + layout.detail_block().map(|(_, h)| h).unwrap_or(0.)
}

/// The natural (pre-shift) y of a slider's row top, from its vertical slot among the present
/// sliders. Output takes slot 0 (right under the system row); the mic slider follows it.
fn slider_row_y(sl: Slider, sliders: Sliders) -> f64 {
    PAD + SYS_H + TILE_GAP + sliders.slot(sl) as f64 * (SLIDER_H + TILE_GAP)
}

/// A slider's row rectangle (full content width). Shifts down under a detail view whose owner sits
/// above it.
fn slider_row_rect(sl: Slider, layout: Layout) -> Rectangle<f64, Logical> {
    let y = slider_row_y(sl, layout.sliders);
    Rectangle::new(
        Point::from((PAD, y + layout.shift_below(y))),
        Size::from((menu_w() - 2. * PAD, SLIDER_H)),
    )
}

/// The mute-button disc at the left of a slider row.
fn slider_icon_rect(sl: Slider, layout: Layout) -> Rectangle<f64, Logical> {
    let row = slider_row_rect(sl, layout);
    Rectangle::new(row.loc, Size::from((SLIDER_H, SLIDER_H)))
}

/// A slider's device-picker menu-button (`go-next` arrow) at the right end of the row —
/// gnome-shell's `QuickSlider._menuButton`, shown **only when there's more than one device** to
/// pick between (`menuEnabled = _deviceItems.size > 1`). Uses the drag-frozen count while this
/// slider is dragged. `None` otherwise.
fn slider_arrow_rect(sl: Slider, layout: Layout) -> Option<Rectangle<f64, Logical>> {
    if layout.arrow_count(sl) <= 1 {
        return None;
    }
    let row = slider_row_rect(sl, layout);
    Some(Rectangle::new(
        Point::from((row.loc.x + row.size.w - ARROW_W, row.loc.y)),
        Size::from((ARROW_W, SLIDER_H)),
    ))
}

/// A slider's interactive track band (row minus the mute disc, and minus the device-picker arrow
/// when shown). The drawn trough is a thin bar centered in this band; the usable handle-center
/// x-range is inset by half a handle so the handle never overhangs. Genuinely shortened when the
/// arrow is present, so an arrow click never lands on the track and `volume_from_x` stays in range.
fn slider_track_rect(sl: Slider, layout: Layout) -> Rectangle<f64, Logical> {
    let row = slider_row_rect(sl, layout);
    let x = row.loc.x + SLIDER_H + SYS_GAP;
    let right = match slider_arrow_rect(sl, layout) {
        Some(arrow) => arrow.loc.x - SYS_GAP,
        None => row.loc.x + row.size.w,
    };
    Rectangle::new(
        Point::from((x, row.loc.y)),
        Size::from((right - x, SLIDER_H)),
    )
}

/// Perceptual volume `0..=1` for a pointer x on a slider's track.
fn volume_from_x(sl: Slider, x: f64, layout: Layout) -> f64 {
    let track = slider_track_rect(sl, layout);
    let left = track.loc.x + SLIDER_HANDLE / 2.;
    let right = track.loc.x + track.size.w - SLIDER_HANDLE / 2.;
    ((x - left) / (right - left)).clamp(0.0, 1.0)
}

/// The x of the handle center for a given perceptual volume on a slider's track.
fn slider_handle_x(sl: Slider, volume: f64, layout: Layout) -> f64 {
    let track = slider_track_rect(sl, layout);
    let left = track.loc.x + SLIDER_HANDLE / 2.;
    let right = track.loc.x + track.size.w - SLIDER_HANDLE / 2.;
    left + volume.clamp(0.0, 1.0) * (right - left)
}

/// The rectangle of tile `i` (row-major), menu-local logical. The grid sits below the top system
/// row (and the volume slider when a sink is present); rows below an open detail view's owner
/// shift down by the card block.
fn tile_rect(i: usize, layout: Layout) -> Rectangle<f64, Logical> {
    let row = (i / COLS) as f64;
    let col = (i % COLS) as f64;
    let x = PAD + col * (TILE_W + TILE_GAP);
    let y = grid_top(layout.sliders) + row * (TILE_H + TILE_GAP);
    Rectangle::new(
        Point::from((x, y + layout.shift_below(y))),
        Size::from((TILE_W, TILE_H)),
    )
}

/// The expand-arrow (menu-button) half of a menu-bearing tile — the right `ARROW_W`, full height —
/// or `None` for a plain toggle. gnome-shell's `.quick-toggle-menu-button`, the second hit region.
/// Takes the tile itself (the grid is dynamic now, so we can't index a global `GRID`).
fn tile_arrow_rect(i: usize, tile: GridTile, layout: Layout) -> Option<Rectangle<f64, Logical>> {
    tile.detail_owner()?;
    // Power Mode's picker arrow shows only with >2 known profiles (gnome-shell's `menuEnabled`);
    // with ≤2 there's nothing to choose, so the tile is body-only (mirrors the slider's >1 gate).
    if matches!(tile, GridTile::PowerProfile) && layout.profile_count <= 2 {
        return None;
    }
    let r = tile_rect(i, layout);
    Some(Rectangle::new(
        Point::from((r.loc.x + r.size.w - ARROW_W, r.loc.y)),
        Size::from((ARROW_W, r.size.h)),
    ))
}

/// The toggle-body half of a tile (the whole tile for a plain toggle; the tile minus the arrow
/// for a menu tile). Its click flips the toggle / opens settings; also the label's clip bound.
fn tile_body_rect(i: usize, tile: GridTile, layout: Layout) -> Rectangle<f64, Logical> {
    let r = tile_rect(i, layout);
    match tile_arrow_rect(i, tile, layout) {
        Some(_) => Rectangle::new(
            r.loc,
            Size::from((r.size.w - ARROW_W - SEPARATOR_W, r.size.h)),
        ),
        None => r,
    }
}

/// The detail-view card rectangle (menu-local logical), pinned below its owner's row, or `None`
/// when collapsed.
fn detail_rect(layout: Layout) -> Option<Rectangle<f64, Logical>> {
    let owner = layout.expanded?;
    let insert_y = owner.anchor_row_bottom(layout.sliders);
    Some(Rectangle::new(
        Point::from((PAD, insert_y + DETAIL_MARGIN)),
        Size::from((
            menu_w() - 2. * PAD,
            owner.detail_height(layout.owner_device_count(owner)),
        )),
    ))
}

/// The rectangle of detail row `k` (0-based, top to bottom), accounting for the header, inter-row
/// gaps, and any group separators above earlier rows. `None` if there's no open detail / no row
/// `k`. Placed from the pure `row_shape` (the same shape `detail_height` sizes the card from), so
/// the geometry can't drift from the drawn rows; a debug_assert at the draw/hit sites ties the
/// live `rows()` to this shape.
fn detail_row_rect(k: usize, layout: Layout) -> Option<Rectangle<f64, Logical>> {
    let owner = layout.expanded?;
    let card = detail_rect(layout)?;
    let shape = owner.row_shape(layout.owner_device_count(owner));
    if k >= shape.len() {
        return None;
    }
    // Walk from the first row's top, adding each earlier row's height + gap, plus a separator's
    // extra space wherever one opens a group.
    let mut y = card.loc.y + DETAIL_PAD + DETAIL_HEADER_H + DETAIL_HEADER_GAP;
    for (j, &separator_before) in shape.iter().enumerate() {
        if j > 0 {
            y += DETAIL_ROW_GAP;
        }
        if separator_before {
            y += DETAIL_SEP_EXTRA;
        }
        if j == k {
            return Some(Rectangle::new(
                Point::from((card.loc.x + DETAIL_PAD, y)),
                Size::from((card.size.w - 2. * DETAIL_PAD, DETAIL_ROW_H)),
            ));
        }
        y += DETAIL_ROW_H;
    }
    None
}

/// The battery pill's rectangle at the far left of the top system row, or `None`
/// when there's no battery (gnome-shell's `PowerToggle.visible = IsPresent`).
fn pill_rect(has_pill: bool) -> Option<Rectangle<f64, Logical>> {
    has_pill.then(|| Rectangle::new(Point::from((PAD, PAD)), Size::from((PILL_W, SYS_H))))
}

/// The hit rectangle of a system button, menu-local. The row is at the top,
/// mirroring gnome-shell's `SystemItem`: with a battery, the pill leads at the
/// left and screenshot/settings/lock/power cluster at the right; without one,
/// screenshot/settings sit on the left and lock/power on the right.
fn sys_rect(button: SysButton, has_pill: bool) -> Rectangle<f64, Logical> {
    // Outermost disc edges align with the tile-grid columns (both inset by PAD).
    let left = PAD + SYS_HIT / 2.;
    let right = menu_w() - PAD - SYS_HIT / 2.;
    let center_x = if has_pill {
        // All four buttons right-aligned (the pill takes the left).
        match button {
            SysButton::Power => right,
            SysButton::Lock => right - SYS_ADVANCE,
            SysButton::Settings => right - 2. * SYS_ADVANCE,
            SysButton::Screenshot => right - 3. * SYS_ADVANCE,
        }
    } else {
        match button {
            SysButton::Screenshot => left,
            SysButton::Settings => left + SYS_ADVANCE,
            SysButton::Lock => right - SYS_ADVANCE,
            SysButton::Power => right,
        }
    };
    Rectangle::new(
        Point::from((center_x - SYS_HIT / 2., PAD)),
        Size::from((SYS_HIT, SYS_H)),
    )
}

/// Resolve the first of `candidates` that rasterizes and build a composited icon
/// element centered at `origin + center` (both menu-local logical), tinted
/// `color`. `None` when no candidate resolves.
#[allow(clippy::too_many_arguments)]
fn icon_element<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    color: [f32; 4],
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
) -> Option<TextureRenderElement<VkTexture>> {
    let buffer = candidates
        .iter()
        .find_map(|name| icons.buffer(name.as_ref(), logical_px, scale, color))?;
    let tb = match TextureBuffer::from_memory_buffer(renderer, &buffer) {
        Ok(tb) => tb,
        Err(err) => {
            tracing::error!("error uploading quick-settings icon: {err:#}");
            return None;
        }
    };
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        1.,
        None,
        None,
        Kind::Unspecified,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_status::KnownProfile;

    fn battery(percentage: f64) -> BatteryStatus {
        BatteryStatus {
            icon_name: "battery-level-80-symbolic".to_string(),
            percentage,
        }
    }

    fn center(r: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
    }

    /// A collapsed layout (no open detail view) with the given output-slider presence (no mic
    /// slider, no devices).
    fn lay(has_slider: bool) -> Layout {
        Layout {
            sliders: Sliders {
                output: has_slider,
                mic: false,
            },
            expanded: None,
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            drag: None,
            grid_len: BASE_GRID.len(),
        }
    }

    /// The tile grid, system row, and (when a sink is present) the volume slider —
    /// which sits between the system row and the grid — lay out inside the menu
    /// without going off an edge, with and without the battery pill.
    #[test]
    fn layout_places_tiles_and_system_row_within_bounds() {
        for audio in [None, Some(AudioStatus::default())] {
            let has_slider = audio.is_some();
            let size = QuickSettings::new(
                QuickToggles::default(),
                NetworkStatus::Wired,
                AirplaneStatus::default(),
                PowerProfileStatus::default(),
                None,
                audio,
                SinkList::default(),
                MicStatus::default(),
                SourceList::default(),
                [0, 0, 0],
            )
            .logical_size();
            let within = |r: Rectangle<f64, Logical>, what: &str| {
                assert!(
                    r.loc.x >= 0. && r.loc.y >= 0.,
                    "{what} off the top/left edge"
                );
                assert!(r.loc.x + r.size.w <= size.w + 0.01, "{what} off the right");
                assert!(r.loc.y + r.size.h <= size.h + 0.01, "{what} off the bottom");
            };
            for i in 0..BASE_GRID.len() {
                within(tile_rect(i, lay(has_slider)), "tile");
            }
            for has_pill in [false, true] {
                for button in SYS_BUTTONS {
                    within(sys_rect(button, has_pill), "sys button");
                }
                if let Some(pill) = pill_rect(has_pill) {
                    within(pill, "battery pill");
                }
            }
            if has_slider {
                let o = Slider::Output;
                within(slider_row_rect(o, lay(has_slider)), "slider row");
                within(slider_icon_rect(o, lay(has_slider)), "slider mute button");
                within(slider_track_rect(o, lay(has_slider)), "slider track");
                // The slider sits above the tile grid.
                assert!(
                    slider_row_rect(o, lay(has_slider)).loc.y
                        + slider_row_rect(o, lay(has_slider)).size.h
                        <= tile_rect(0, lay(has_slider)).loc.y + 0.01,
                    "slider must be above the first tile row"
                );
            }
        }
    }

    /// Clicking a gsettings tile flips its state and returns the matching set-action.
    #[test]
    fn clicking_a_tile_flips_and_returns_the_action() {
        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );
        let dnd = tile_rect(2, lay(false)); // grid: [Network, Dark Style, Do Not Disturb, Night Light]
        let before = qs.revision;
        let action = qs.pointer_click(center(dnd));
        assert!(matches!(action, PopoverAction::SetDoNotDisturb(true)));
        assert!(qs.toggles.do_not_disturb);
        assert!(qs.revision > before);
    }

    /// The menu tracks the hovered control (tile / system button) and clears it
    /// when the pointer leaves; each change bumps the revision so the chrome
    /// re-bakes with the highlight, and re-hovering the same control is a no-op.
    #[test]
    fn hover_tracks_controls() {
        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );
        let rev0 = qs.revision;
        assert!(qs.pointer_hover(Some(center(tile_rect(1, lay(false))))));
        assert_eq!(qs.hovered, Some(QsHover::Tile(1)));
        assert!(qs.revision > rev0, "a hover change bumps the revision");

        let rev1 = qs.revision;
        assert!(!qs.pointer_hover(Some(center(tile_rect(1, lay(false))))));
        assert_eq!(qs.revision, rev1, "re-hovering the same tile is a no-op");

        assert!(qs.pointer_hover(Some(center(sys_rect(SysButton::Settings, false)))));
        assert_eq!(qs.hovered, Some(QsHover::Sys(SysButton::Settings)));

        assert!(qs.pointer_hover(None), "leaving the menu clears the hover");
        assert_eq!(qs.hovered, None);
    }

    /// The Network tile (grid cell 0) reads as "on" when connected and, clicked,
    /// opens network settings without flipping any local toggle state.
    #[test]
    fn network_tile_reflects_state_and_opens_settings() {
        let off = AirplaneStatus::default();
        let np = PowerProfileStatus::default();
        assert!(GridTile::Network.is_on(QuickToggles::default(), NetworkStatus::Wired, off, &np));
        assert!(GridTile::Network.is_on(
            QuickToggles::default(),
            NetworkStatus::Wireless(60),
            off,
            &np
        ));
        assert!(!GridTile::Network.is_on(
            QuickToggles::default(),
            NetworkStatus::Offline,
            off,
            &np
        ));

        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );
        let before = qs.revision;
        // The tile center falls in the toggle-body (left of the arrow-half), which opens settings.
        let action = qs.pointer_click(center(tile_rect(0, lay(false))));
        match action {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-control-center", "network"]),
            other => panic!("expected network settings, got {other:?}"),
        }
        // Network is not a gsettings toggle: no local flip, so no chrome revision bump.
        assert_eq!(qs.revision, before);
    }

    /// The screenshot button opens the UI; the settings button spawns its command.
    #[test]
    fn clicking_system_buttons_returns_their_actions() {
        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );

        let shot = qs.pointer_click(center(sys_rect(SysButton::Screenshot, false)));
        assert!(matches!(shot, PopoverAction::Screenshot));

        let settings = qs.pointer_click(center(sys_rect(SysButton::Settings, false)));
        match settings {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd[0], "gnome-control-center"),
            other => panic!("expected a spawn, got {other:?}"),
        }
    }

    /// The battery pill only exists with a battery, and clicking it opens power
    /// settings. Its presence also shifts the system buttons right (they cluster).
    #[test]
    fn battery_pill_appears_and_opens_power_settings() {
        assert!(pill_rect(false).is_none());

        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            Some(battery(79.)),
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );
        let pill = pill_rect(true).expect("a battery must show the pill");
        let action = qs.pointer_click(center(pill));
        match action {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-control-center", "power"]),
            other => panic!("expected power settings, got {other:?}"),
        }
        // Settings sits further right when the pill is present.
        assert!(
            sys_rect(SysButton::Settings, true).loc.x > sys_rect(SysButton::Settings, false).loc.x
        );
    }

    /// A click in empty menu space is consumed but does nothing.
    #[test]
    fn clicking_empty_space_is_consumed() {
        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );
        // Below the top system row, between the two tile columns.
        let action = qs.pointer_click(Point::from((menu_w() / 2., PAD + SYS_H + 2.)));
        assert!(matches!(action, PopoverAction::Consumed));
    }

    /// Draw the chrome into an offscreen and read it back: an opaque dark menu
    /// with the active tile painted the accent color and visible label ink.
    #[test]
    fn draws_the_menu_with_an_active_tile() {
        use smithay::backend::renderer::{ExportMem, Texture as _};

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_the_menu_with_an_active_tile: no Vulkan device ({e})");
                return;
            }
        };
        // Dark Style on, with a vivid red accent so the active tile is unmistakable.
        // Network Unknown → the Network tile (cell 0) stays grey, not accent.
        let toggles = QuickToggles {
            dark_style: true,
            ..Default::default()
        };
        let qs = QuickSettings::new(
            toggles,
            NetworkStatus::Unknown,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            Some(battery(79.)),
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0xff, 0x00, 0x00],
        );
        let mut tex = qs.draw(&mut vk, 1.).expect("menu texture");
        let size = tex.size();

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // The menu's outer corner is rounded away: the top-left pixel is transparent.
        let tl = [pixels[0], pixels[1], pixels[2], pixels[3]];
        assert!(
            tl[3] < 40,
            "the menu's outer corner must be transparent (rounded), got {tl:?}"
        );

        // The center of the Dark Style tile (grid cell 1) is the accent: high R, low G/B.
        let r0 = tile_rect(1, lay(false));
        let cx = (r0.loc.x + r0.size.w * 0.15) as i32;
        let cy = (r0.loc.y + r0.size.h / 2.) as i32;
        let i = ((cy * size.w + cx) * 4) as usize;
        let px = [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]];
        assert!(
            px[0] > 150 && px[1] < 80 && px[2] < 80 && px[3] > 150,
            "the active Dark Style tile must be the accent color, got {px:?}"
        );

        // The tile's corner is cut to the pill: a deep-corner pixel reveals the dark MENU_BG
        // beneath, not the accent — proving `render_rounded_rect` rounded the tile in the
        // offscreen.
        let kx = (r0.loc.x + 2.) as i32;
        let ky = (r0.loc.y + 2.) as i32;
        let k = ((ky * size.w + kx) * 4) as usize;
        let corner = [pixels[k], pixels[k + 1], pixels[k + 2]];
        assert!(
            corner[0] < 70 && corner[1] < 70 && corner[2] < 70,
            "the active tile's corner must be cut to MENU_BG, got {corner:?}"
        );

        // The Night Light tile (grid cell 3, off) is the dim grey, not the accent.
        let r2 = tile_rect(3, lay(false));
        let gx = (r2.loc.x + r2.size.w * 0.15) as i32;
        let gy = (r2.loc.y + r2.size.h / 2.) as i32;
        let j = ((gy * size.w + gx) * 4) as usize;
        let g = [pixels[j], pixels[j + 1], pixels[j + 2]];
        assert!(
            g[0] < 100 && g[1] < 100 && g[2] < 100,
            "an inactive tile must be dim grey, got {g:?}"
        );
    }

    fn qs(network: NetworkStatus, audio: Option<AudioStatus>) -> QuickSettings {
        QuickSettings::new(
            QuickToggles::default(),
            network,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            None,
            audio,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        )
    }

    /// `n` output sinks with `sink0` the default, and a bound sink (so the slider — and thus the
    /// picker arrow — exist).
    fn make_sinks(n: usize) -> SinkList {
        SinkList {
            sinks: (0..n)
                .map(|i| crate::audio::SinkInfo {
                    name: format!("sink{i}"),
                    description: format!("Sink {i}"),
                })
                .collect(),
            default_name: (n > 0).then(|| "sink0".to_string()),
        }
    }

    /// A quick-settings menu with a live volume slider and `n` output sinks.
    fn qs_with_sinks(n: usize) -> QuickSettings {
        let mut q = qs(NetworkStatus::Wired, Some(AudioStatus::default()));
        q.sink_list = make_sinks(n);
        q
    }

    /// `n` input sources with `source0` the default.
    fn make_sources(n: usize) -> SourceList {
        SourceList {
            sources: (0..n)
                .map(|i| crate::audio::SourceInfo {
                    name: format!("source{i}"),
                    description: format!("Source {i}"),
                })
                .collect(),
            default_name: (n > 0).then(|| "source0".to_string()),
        }
    }

    /// A recording mic with a bound source at half volume — the mic slider is visible.
    fn recording_mic() -> MicStatus {
        MicStatus {
            recording: true,
            muted: false,
            volume: 0.5,
            source_present: true,
        }
    }

    /// A menu with BOTH sliders live (one output sink + a recording mic) and `n` input sources.
    fn qs_with_sources(n: usize) -> QuickSettings {
        let mut q = qs(NetworkStatus::Wired, Some(AudioStatus::default()));
        q.mic = recording_mic();
        q.source_list = make_sources(n);
        q
    }

    /// The mic slider shows only while recording with a bound source (gnome-shell's
    /// `stream != null && recording`); it stacks directly below the output slider, above the grid.
    #[test]
    fn mic_slider_shows_only_while_recording_with_a_source() {
        // Recording but no bound source → no mic slider.
        let mut q = qs(NetworkStatus::Wired, Some(AudioStatus::default()));
        q.mic = MicStatus {
            recording: true,
            source_present: false,
            ..MicStatus::default()
        };
        assert!(!q.sliders().mic, "no slider without a bound source");
        // Bound source but not recording → still no mic slider.
        q.mic = MicStatus {
            recording: false,
            source_present: true,
            ..MicStatus::default()
        };
        assert!(!q.sliders().mic, "no slider when not recording");
        // Recording + bound source → the mic slider appears, below the output slider.
        let q = qs_with_sources(1);
        assert!(q.sliders().mic && q.sliders().output);
        let out = slider_row_rect(Slider::Output, q.layout());
        let mic = slider_row_rect(Slider::Mic, q.layout());
        assert!(
            (mic.loc.y - (out.loc.y + SLIDER_H + TILE_GAP)).abs() < 0.01,
            "mic slider must sit one row below the output slider"
        );
        assert!(
            mic.loc.y + mic.size.h <= tile_rect(0, q.layout()).loc.y + 0.01,
            "mic slider must be above the first tile row"
        );
    }

    /// The mic slider's icon toggles the source mute; its track sets the source volume — distinct
    /// actions from the output slider so the two never cross-wire.
    #[test]
    fn mic_slider_icon_and_track_return_input_actions() {
        let mut q = qs_with_sources(1);
        let layout = q.layout();
        assert!(matches!(
            q.pointer_click(center(slider_icon_rect(Slider::Mic, layout))),
            PopoverAction::ToggleInputMute
        ));
        let track = slider_track_rect(Slider::Mic, layout);
        match q.pointer_click(center(track)) {
            PopoverAction::SetInputVolume(v) => assert!((v - 0.5).abs() < 0.05),
            other => panic!("expected SetInputVolume, got {other:?}"),
        }
        // The output slider still returns output actions.
        assert!(matches!(
            q.pointer_click(center(slider_icon_rect(Slider::Output, q.layout()))),
            PopoverAction::ToggleMute
        ));
    }

    /// The input picker arrow shows only with >1 source; opening it lists the sources (default
    /// checked) then "Sound Settings", and a row sets that source default.
    #[test]
    fn input_picker_lists_sources_and_routes_actions() {
        assert!(slider_arrow_rect(Slider::Mic, qs_with_sources(1).layout()).is_none());
        let mut q = qs_with_sources(3); // default = source0
        let arrow = slider_arrow_rect(Slider::Mic, q.layout()).expect("arrow with >1 source");
        assert!(matches!(
            q.pointer_click(center(arrow)),
            PopoverAction::Consumed
        ));
        assert_eq!(q.expanded, Some(DetailOwner::Input));

        let rows = DetailOwner::Input.rows(q.network, &q.sink_list, &q.source_list, &q.power);
        assert_eq!(rows.len(), 4, "3 sources + Sound Settings");
        assert!(rows[0].selected && !rows[1].selected);
        assert_eq!(rows[3].label, "Sound Settings");
        match q.pointer_click(center(detail_row_rect(1, q.layout()).unwrap())) {
            PopoverAction::SetDefaultSource(name) => assert_eq!(name, "source1"),
            other => panic!("expected SetDefaultSource, got {other:?}"),
        }
    }

    /// The input picker's card pins below the mic slider row (which is itself below the output
    /// slider), and opening it while the output picker is open replaces it (one detail at a time).
    #[test]
    fn input_picker_anchors_below_the_mic_slider_and_is_exclusive() {
        let mut q = qs_with_sources(2);
        q.sink_list = make_sinks(2); // give the output picker an arrow too
                                     // Open the output picker first.
        let out_arrow = slider_arrow_rect(Slider::Output, q.layout()).unwrap();
        q.pointer_click(center(out_arrow));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        // Opening the input picker replaces it.
        let mic_arrow = slider_arrow_rect(Slider::Mic, q.layout()).unwrap();
        q.pointer_click(center(mic_arrow));
        assert_eq!(q.expanded, Some(DetailOwner::Input));
        // The card sits below the mic slider's row bottom.
        let mic = slider_row_rect(Slider::Mic, q.layout());
        let card = detail_rect(q.layout()).unwrap();
        assert!(card.loc.y >= mic.loc.y + SLIDER_H - 0.01);
    }

    /// An open input picker collapses when recording stops (its slider vanishes) or sources drop to
    /// one — the same `normalize_expanded` guard the output picker uses.
    #[test]
    fn input_picker_collapses_when_its_owner_vanishes() {
        let mut q = qs_with_sources(2);
        q.pointer_click(center(slider_arrow_rect(Slider::Mic, q.layout()).unwrap()));
        assert_eq!(q.expanded, Some(DetailOwner::Input));
        // Recording stops → slider (and picker) gone.
        assert!(q.set_mic(MicStatus::default()));
        assert!(q.expanded.is_none());

        // And the sources-drop-to-one path.
        let mut q = qs_with_sources(2);
        q.pointer_click(center(slider_arrow_rect(Slider::Mic, q.layout()).unwrap()));
        assert!(q.set_source_list(make_sources(1)));
        assert!(q.expanded.is_none());
    }

    /// A source hot-plugging mid-drag must not snap the mic volume (the dragged slider's arrow is
    /// frozen), and recording stopping mid-drag cancels the drag rather than stranding it.
    #[test]
    fn mic_drag_freezes_geometry_and_recording_stop_cancels_it() {
        let mut q = qs_with_sources(1); // one source → no mic arrow, full-width track
        let track = slider_track_rect(Slider::Mic, q.layout());
        let press = Point::from((
            track.loc.x + track.size.w * 0.5,
            track.loc.y + track.size.h / 2.,
        ));
        q.pointer_click(press);
        assert!(matches!(q.sliding, Some((Slider::Mic, _))));
        let held = q.mic.volume;

        // A second source appears mid-drag: the arrow stays suppressed, the level doesn't snap.
        assert!(q.set_source_list(make_sources(2)));
        assert!(slider_arrow_rect(Slider::Mic, q.layout()).is_none());
        assert!(matches!(
            q.pointer_drag(press).unwrap(),
            PopoverAction::SetInputVolume(_)
        ));
        assert_eq!(
            q.mic.volume, held,
            "a stationary pointer must not snap the mic level"
        );
        assert!(
            q.end_drag(),
            "the mid-drag hotplug needs a redraw on release"
        );

        // Recording stopping mid-drag cancels the drag (no lingering slider emitting at a gone
        // src).
        let mut q = qs_with_sources(1);
        let track = slider_track_rect(Slider::Mic, q.layout());
        q.pointer_click(center(track));
        assert!(q.sliding.is_some());
        assert!(q.set_mic(MicStatus::default()));
        assert!(q.sliding.is_none(), "recording stop must cancel the drag");
        assert!(!q.sliders().mic, "and hide the slider");
    }

    /// The output slider gets the same vanish-cancels-the-drag treatment as the mic (Fable M1): a
    /// sink unplugged mid-drag adopts `None`, cancels the drag, and hides the slider — rather than
    /// stranding a dead slider (the event-driven `None` is never re-sent).
    #[test]
    fn output_slider_drag_is_cancelled_when_the_sink_vanishes() {
        let mut q = qs_with_sinks(1);
        let track = slider_track_rect(Slider::Output, q.layout());
        q.pointer_click(center(track));
        assert!(matches!(q.sliding, Some((Slider::Output, _))));
        assert!(
            q.set_audio(None),
            "the sink unplugging must be adopted mid-drag"
        );
        assert!(q.sliding.is_none(), "and cancel the drag");
        assert!(!q.sliders().output, "and hide the slider");
    }

    /// A recording mic with no bound output sink is the *only* slider: it takes the top slot (where
    /// a lone output slider would sit), and its picker anchors below it with no output slider
    /// above.
    #[test]
    fn mic_only_slider_takes_the_top_slot() {
        let mut q = qs(NetworkStatus::Wired, None); // audio None → no output slider
        q.mic = recording_mic();
        q.source_list = make_sources(2);
        assert!(q.sliders().mic && !q.sliders().output);
        let mic = slider_row_rect(Slider::Mic, q.layout());
        assert!(
            (mic.loc.y - (PAD + SYS_H + TILE_GAP)).abs() < 0.01,
            "the lone mic slider sits in the top slot"
        );
        assert!(mic.loc.y + mic.size.h <= tile_rect(0, q.layout()).loc.y + 0.01);
        q.pointer_click(center(slider_arrow_rect(Slider::Mic, q.layout()).unwrap()));
        assert_eq!(q.expanded, Some(DetailOwner::Input));
        let card = detail_rect(q.layout()).unwrap();
        assert!(card.loc.y >= mic.loc.y + SLIDER_H - 0.01);
    }

    /// A menu tile's arrow-half and toggle-body are disjoint hit regions; a plain toggle has no
    /// arrow and its body is the whole tile.
    #[test]
    fn tile_body_and_arrow_are_disjoint_regions() {
        let l = lay(false);
        let ni = network_index();
        let body = tile_body_rect(ni, GridTile::Network, l);
        let arrow =
            tile_arrow_rect(ni, GridTile::Network, l).expect("the Network tile carries an arrow");
        // The body ends at (or before) the arrow's left edge — a separator sits between.
        assert!(body.loc.x + body.size.w <= arrow.loc.x);
        assert_eq!(arrow.loc.x + arrow.size.w, tile_rect(ni, l).loc.x + TILE_W);
        // A gsettings toggle (Do Not Disturb, cell 2) is all body, no arrow.
        assert!(tile_arrow_rect(2, BASE_GRID[2], l).is_none());
        assert_eq!(tile_body_rect(2, BASE_GRID[2], l), tile_rect(2, l));
    }

    /// With rfkill hardware present the grid gains a 5th tile (Airplane Mode), always APPENDED at
    /// index 4: a lone tile on a new third row (row 2, column 0). The menu grows by exactly one
    /// tile row; the empty cell beside it (row 2, column 1) carries no tile and swallows clicks;
    /// and clicking the Airplane body returns the non-optimistic D-Bus write — hit geometry agrees
    /// with the live grid.
    #[test]
    fn airplane_tile_appends_a_third_row_and_toggles() {
        // show = true appends the tile; active = false so a click turns it on.
        let airplane = AirplaneStatus {
            active: false,
            show: true,
        };
        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            airplane,
            PowerProfileStatus::default(),
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );

        // Five tiles now, Airplane last (the append invariant network_index() depends on).
        let tiles = qs.grid();
        assert_eq!(tiles.len(), 5);
        assert_eq!(tiles[4], GridTile::Airplane);

        // The 5th tile sits alone on a new row 2, column 0.
        let air = tile_rect(4, qs.layout());
        let row0 = tile_rect(0, qs.layout());
        assert!(
            (air.loc.x - row0.loc.x).abs() < 0.01,
            "airplane in column 0"
        );
        let expected_y = row0.loc.y + 2. * (TILE_H + TILE_GAP);
        assert!(
            (air.loc.y - expected_y).abs() < 0.01,
            "airplane on the third row"
        );

        // The menu is exactly one tile row taller than the 4-tile grid (same sliders/detail state).
        let grew = menu_h(qs.layout()) - menu_h(lay(false));
        assert!(
            (grew - (TILE_H + TILE_GAP)).abs() < 0.01,
            "one extra tile row, got {grew}"
        );

        // Clicking the Airplane body returns the D-Bus write; echo-driven, so no local flip.
        let action = qs.pointer_click(center(air));
        assert!(matches!(action, PopoverAction::SetAirplaneMode(true)));
        assert!(!qs.airplane.active, "the tile must not flip optimistically");

        // The empty cell beside it (row 2, column 1) is grid space with no tile: consumed, no-op.
        let empty = Point::from((
            tile_rect(1, qs.layout()).loc.x + TILE_W / 2.,
            air.loc.y + TILE_H / 2.,
        ));
        assert!(matches!(qs.pointer_click(empty), PopoverAction::Consumed));
    }

    /// With power-profiles-daemon present the grid gains a Power Mode tile, appended as the FIRST
    /// conditional (index 4, before Airplane). It's a two-line tile ("Power Mode" + the active
    /// profile subtitle), reads "on" when not Balanced, and its body-click returns the (target-
    /// deferred) toggle action. With both conditional tiles shown, PowerProfile stays ahead of
    /// Airplane, the invariant `power_profile_index`/`anchor_row_bottom` depend on.
    #[test]
    fn power_mode_tile_appends_with_subtitle_and_body_toggles() {
        let power = PowerProfileStatus {
            active: "performance".to_string(),
            available: vec![
                KnownProfile::Performance,
                KnownProfile::Balanced,
                KnownProfile::PowerSaver,
            ],
            show: true,
        };
        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            power,
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        );

        // Appended as the 5th tile at the constant power_profile_index (4).
        let tiles = qs.grid();
        assert_eq!(tiles.len(), 5);
        assert_eq!(power_profile_index(), 4);
        assert_eq!(tiles[power_profile_index()], GridTile::PowerProfile);

        // Two-line tile: static "Power Mode" title, active profile as the subtitle.
        assert_eq!(
            GridTile::PowerProfile.label(NetworkStatus::Wired),
            "Power Mode"
        );
        assert_eq!(
            GridTile::PowerProfile.subtitle(&qs.power).as_deref(),
            Some("Performance")
        );
        // On because the active profile isn't Balanced.
        assert!(GridTile::PowerProfile.is_on(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            &qs.power,
        ));

        // Body click returns the toggle action; no arrow yet, so the whole tile is body.
        let action = qs.pointer_click(center(tile_rect(power_profile_index(), qs.layout())));
        assert!(matches!(action, PopoverAction::TogglePowerProfile));

        // Both conditionals shown: PowerProfile (4) stays ahead of Airplane (5).
        qs.set_airplane(AirplaneStatus {
            active: false,
            show: true,
        });
        let tiles = qs.grid();
        assert_eq!(tiles.len(), 6);
        assert_eq!(tiles[4], GridTile::PowerProfile);
        assert_eq!(tiles[5], GridTile::Airplane);
    }

    /// A QS with `n` known profiles present, `active` selected, daemon shown.
    fn qs_profiles(active: &str, profiles: &[KnownProfile]) -> QuickSettings {
        QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus {
                active: active.to_string(),
                available: profiles.to_vec(),
                show: true,
            },
            None,
            None,
            SinkList::default(),
            MicStatus::default(),
            SourceList::default(),
            [0, 0, 0],
        )
    }

    /// The Power Mode picker: its arrow shows only with >2 profiles (gnome-shell's `menuEnabled`),
    /// opening it lists the known profiles (reversed, active checked) + a Power Settings row, a row
    /// click sets that profile, and a drop to ≤2 profiles collapses an open picker.
    #[test]
    fn power_mode_picker_lists_profiles_and_gates_on_count() {
        let all = [
            KnownProfile::Performance,
            KnownProfile::Balanced,
            KnownProfile::PowerSaver,
        ];
        let ppi = power_profile_index();

        // >2 profiles → an arrow that opens the picker.
        let mut qs = qs_profiles("performance", &all);
        let arrow =
            tile_arrow_rect(ppi, GridTile::PowerProfile, qs.layout()).expect("an arrow with 3");
        assert!(matches!(
            qs.pointer_click(center(arrow)),
            PopoverAction::Consumed
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::PowerProfile));

        // Rows: 3 profiles (reversed: performance→power-saver) + Power Settings; active checked.
        let rows =
            DetailOwner::PowerProfile.rows(qs.network, &qs.sink_list, &qs.source_list, &qs.power);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].label, "Performance");
        assert_eq!(rows[2].label, "Power Saver");
        assert!(rows[0].selected && !rows[1].selected);
        assert!(rows[3].separator_before, "Power Settings past a separator");

        // Clicking the Power Saver row sets that profile (echo-driven, menu stays open).
        let row = detail_row_rect(2, qs.layout()).expect("the power-saver row");
        match qs.pointer_click(center(row)) {
            PopoverAction::SetPowerProfile(id) => assert_eq!(id, "power-saver"),
            other => panic!("expected SetPowerProfile, got {other:?}"),
        }

        // ≤2 profiles → no arrow (menuEnabled off); the whole tile is the body toggle.
        let mut qs2 = qs_profiles("balanced", &all[1..]);
        assert!(tile_arrow_rect(ppi, GridTile::PowerProfile, qs2.layout()).is_none());
        assert!(matches!(
            qs2.pointer_click(center(tile_rect(ppi, qs2.layout()))),
            PopoverAction::TogglePowerProfile
        ));

        // Collapse-on-vanish: open the picker, then drop to 2 profiles → it closes.
        let mut qs3 = qs_profiles("performance", &all);
        qs3.pointer_click(center(
            tile_arrow_rect(ppi, GridTile::PowerProfile, qs3.layout()).unwrap(),
        ));
        assert_eq!(qs3.expanded, Some(DetailOwner::PowerProfile));
        qs3.set_power_profile(PowerProfileStatus {
            active: "balanced".to_string(),
            available: all[1..].to_vec(),
            show: true,
        });
        assert!(
            qs3.expanded.is_none(),
            "picker must collapse when profiles drop to <=2"
        );
    }

    /// With BOTH conditional tiles shown, the Power Mode picker still anchors below the Power tile
    /// (its constant index-4 row), not the Airplane tile beside it — the append-order invariant
    /// that `power_profile_index`/`anchor_row_bottom` depend on. (Guards Fable's
    /// conditional-append trap.)
    #[test]
    fn power_picker_anchors_below_its_tile_with_both_conditionals() {
        let mut qs = qs_profiles(
            "performance",
            &[
                KnownProfile::Performance,
                KnownProfile::Balanced,
                KnownProfile::PowerSaver,
            ],
        );
        qs.set_airplane(AirplaneStatus {
            active: false,
            show: true,
        });
        let ppi = power_profile_index();
        assert_eq!(qs.grid().len(), 6, "power + airplane both shown");

        qs.pointer_click(center(
            tile_arrow_rect(ppi, GridTile::PowerProfile, qs.layout()).unwrap(),
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::PowerProfile));
        let card = detail_rect(qs.layout()).expect("a card when expanded");
        let tile = tile_rect(ppi, qs.layout());
        assert!(
            card.loc.y >= tile.loc.y + TILE_H - 0.01,
            "card must pin below the Power Mode tile row, not elsewhere"
        );
    }

    /// The Network arrow opens the detail view; clicking it again collapses it. Both are internal
    /// state changes (Consumed) that bump the chrome revision.
    #[test]
    fn network_arrow_toggles_the_detail_view() {
        let mut qs = qs(NetworkStatus::Wired, None);
        let ni = network_index();
        assert!(qs.expanded.is_none());

        let before = qs.revision;
        let a = qs.pointer_click(center(
            tile_arrow_rect(ni, GridTile::Network, qs.layout()).unwrap(),
        ));
        assert!(matches!(a, PopoverAction::Consumed));
        assert_eq!(qs.expanded, Some(DetailOwner::Network));
        assert!(qs.revision > before);

        // Network is a row-0 tile, so opening its detail doesn't shift it — the arrow stays put.
        let a = qs.pointer_click(center(
            tile_arrow_rect(ni, GridTile::Network, qs.layout()).unwrap(),
        ));
        assert!(matches!(a, PopoverAction::Consumed));
        assert!(qs.expanded.is_none());
    }

    /// Opening a detail view grows the menu (taller, same width) and shifts only the rows below
    /// the owner's row; the card sits between the owner's row and the shifted rows.
    #[test]
    fn detail_view_grows_menu_and_shifts_lower_rows_only() {
        let collapsed = Layout {
            sliders: Sliders {
                output: false,
                mic: false,
            },
            expanded: None,
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            drag: None,
            grid_len: BASE_GRID.len(),
        };
        let expanded = Layout {
            sliders: Sliders {
                output: false,
                mic: false,
            },
            expanded: Some(DetailOwner::Network),
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            drag: None,
            grid_len: BASE_GRID.len(),
        };
        assert!(
            menu_h(expanded) > menu_h(collapsed),
            "the menu must grow taller"
        );

        let block = DETAIL_MARGIN + DetailOwner::Network.detail_height(0);
        // Row 0 (Network + its neighbor) does not move.
        for i in 0..COLS {
            assert_eq!(
                tile_rect(i, expanded).loc.y,
                tile_rect(i, collapsed).loc.y,
                "row 0 must not shift"
            );
        }
        // Row 1 shifts down by exactly the detail block.
        for i in COLS..BASE_GRID.len() {
            let d = tile_rect(i, expanded).loc.y - tile_rect(i, collapsed).loc.y;
            assert!(
                (d - block).abs() < 0.01,
                "row 1 must shift by the block, got {d}"
            );
        }
        // The card is pinned below the Network row and clears the shifted next row.
        let card = detail_rect(expanded).expect("a card when expanded");
        let ni = network_index();
        assert!(card.loc.y >= tile_rect(ni, expanded).loc.y + TILE_H - 0.01);
        assert!(card.loc.y + card.size.h <= tile_rect(COLS, expanded).loc.y + 0.01);
        // Never wider than the collapsed menu (keeps the popover origin stable on expand).
        assert!(card.loc.x + card.size.w <= menu_w() - PAD + 0.01);
    }

    /// With the Network detail open, clicking its row runs the row's action (a settings spawn,
    /// which closes the menu); the row lies inside the card.
    #[test]
    fn detail_row_runs_its_action() {
        let mut qs = qs(NetworkStatus::Wired, None);
        qs.pointer_click(center(
            tile_arrow_rect(network_index(), GridTile::Network, qs.layout()).unwrap(),
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::Network));

        let row = detail_row_rect(0, qs.layout()).expect("one detail row");
        let card = detail_rect(qs.layout()).unwrap();
        assert!(
            card.contains(row.loc) && card.contains(center(row)),
            "row must lie in the card"
        );

        let action = qs.pointer_click(center(row));
        match action {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-control-center", "network"]),
            other => panic!("expected the network-settings spawn, got {other:?}"),
        }
    }

    /// A click in the card but not on a row is swallowed (stays open); a click outside the whole
    /// menu is the shell's concern, not ours.
    #[test]
    fn clicking_card_gutter_is_consumed_and_keeps_the_detail_open() {
        let mut qs = qs(NetworkStatus::Wired, None);
        qs.pointer_click(center(
            tile_arrow_rect(network_index(), GridTile::Network, qs.layout()).unwrap(),
        ));
        let card = detail_rect(qs.layout()).unwrap();
        // The header strip (above the first row) is card space with no action.
        let header = Point::from((card.loc.x + card.size.w - 4., card.loc.y + 2.));
        let a = qs.pointer_click(header);
        assert!(matches!(a, PopoverAction::Consumed));
        assert_eq!(
            qs.expanded,
            Some(DetailOwner::Network),
            "the detail must stay open"
        );
    }

    /// Even with a slider present and the detail open, the menu height stays modest — documents
    /// the "no scroll needed, never clips a normal display" assumption (the popover doesn't clamp
    /// its bottom edge).
    #[test]
    fn expanded_menu_stays_within_a_sane_height() {
        for owner in [DetailOwner::Network, DetailOwner::Power] {
            for has_slider in [false, true] {
                let l = Layout {
                    sliders: Sliders {
                        output: has_slider,
                        mic: false,
                    },
                    expanded: Some(owner),
                    sink_count: 0,
                    source_count: 0,
                    profile_count: 0,
                    drag: None,
                    grid_len: BASE_GRID.len(),
                };
                assert!(
                    menu_h(l) < 600.,
                    "expanded menu unexpectedly tall: {}",
                    menu_h(l)
                );
            }
        }
    }

    /// The system-row power button opens a session submenu (gnome-shell's `ShutdownItem`) rather
    /// than powering off directly; its rows are the session actions, Log Out past a group break.
    #[test]
    fn power_button_opens_the_session_submenu() {
        let mut qs = qs(NetworkStatus::Wired, None);
        let before = qs.revision;
        let a = qs.pointer_click(center(sys_rect(SysButton::Power, false)));
        assert!(
            matches!(a, PopoverAction::Consumed),
            "opens the menu, not a spawn"
        );
        assert_eq!(qs.expanded, Some(DetailOwner::Power));
        assert!(qs.revision > before);

        let (_, title) = DetailOwner::Power.header(NetworkStatus::Unknown);
        assert_eq!(title, "Power Off");
        let rows = DetailOwner::Power.rows(
            NetworkStatus::Unknown,
            &SinkList::default(),
            &SourceList::default(),
            &PowerProfileStatus::default(),
        );
        assert_eq!(rows.len(), 4);
        assert!(rows[3].separator_before, "Log Out starts the session group");

        // The "Power Off…" row (index 2) spawns the shutdown command.
        let row = detail_row_rect(2, qs.layout()).expect("the power-off row");
        match qs.pointer_click(center(row)) {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-session-quit", "--power-off"]),
            other => panic!("expected the power-off spawn, got {other:?}"),
        }
    }

    /// At most one detail view is open: opening a second closes the first (across owners).
    #[test]
    fn only_one_detail_view_is_open_at_a_time() {
        let mut qs = qs(NetworkStatus::Wired, None);
        qs.pointer_click(center(
            tile_arrow_rect(network_index(), GridTile::Network, qs.layout()).unwrap(),
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::Network));
        // The power button is in the top row (never shifted), so it's still hittable; opening it
        // replaces the Network detail rather than stacking.
        qs.pointer_click(center(sys_rect(SysButton::Power, false)));
        assert_eq!(qs.expanded, Some(DetailOwner::Power));
    }

    /// The power detail pins below the system row: the row itself doesn't move, but the slider and
    /// the whole grid shift down by the card block.
    #[test]
    fn power_detail_shifts_slider_and_grid_but_not_the_system_row() {
        let collapsed = Layout {
            sliders: Sliders {
                output: true,
                mic: false,
            },
            expanded: None,
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            drag: None,
            grid_len: BASE_GRID.len(),
        };
        let expanded = Layout {
            sliders: Sliders {
                output: true,
                mic: false,
            },
            expanded: Some(DetailOwner::Power),
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            drag: None,
            grid_len: BASE_GRID.len(),
        };
        let block = DETAIL_MARGIN + DetailOwner::Power.detail_height(0);
        assert_eq!(
            sys_rect(SysButton::Power, false).loc.y,
            PAD,
            "system row must not move"
        );
        let ds = slider_row_rect(Slider::Output, expanded).loc.y
            - slider_row_rect(Slider::Output, collapsed).loc.y;
        assert!(
            (ds - block).abs() < 0.01,
            "the slider must shift by the block, got {ds}"
        );
        for i in 0..BASE_GRID.len() {
            let d = tile_rect(i, expanded).loc.y - tile_rect(i, collapsed).loc.y;
            assert!(
                (d - block).abs() < 0.01,
                "tile {i} must shift by the block, got {d}"
            );
        }
        let card = detail_rect(expanded).unwrap();
        assert!((card.loc.y - (PAD + SYS_H + DETAIL_MARGIN)).abs() < 0.01);
    }

    /// The output-device picker arrow shows only with more than one sink (gnome-shell's `>1` gate);
    /// when shown it genuinely shortens the track so an arrow click can't start a volume drag.
    #[test]
    fn slider_arrow_appears_only_with_multiple_sinks() {
        for (n, expect) in [(0, false), (1, false), (2, true), (5, true)] {
            assert_eq!(
                slider_arrow_rect(Slider::Output, qs_with_sinks(n).layout()).is_some(),
                expect,
                "n={n}"
            );
        }
        let one = qs_with_sinks(1);
        let two = qs_with_sinks(2);
        let t1 = slider_track_rect(Slider::Output, one.layout());
        let t2 = slider_track_rect(Slider::Output, two.layout());
        assert!(
            t2.loc.x + t2.size.w < t1.loc.x + t1.size.w,
            "the track must shrink to make room for the arrow"
        );
        // volume_from_x still maps the full 0..1 within the shortened track.
        let l = two.layout();
        let track = slider_track_rect(Slider::Output, l);
        assert!(volume_from_x(Slider::Output, track.loc.x, l).abs() < 1e-9);
        assert!((volume_from_x(Slider::Output, track.loc.x + track.size.w, l) - 1.0).abs() < 1e-9);
    }

    /// The slider arrow toggles the output picker and never moves the volume; the slider row (its
    /// owner) doesn't shift, so the arrow stays hittable to close it.
    #[test]
    fn slider_arrow_toggles_the_picker_without_changing_volume() {
        let mut q = qs_with_sinks(2);
        let before = q.audio.unwrap().volume;
        let a = q.pointer_click(center(
            slider_arrow_rect(Slider::Output, q.layout()).unwrap(),
        ));
        assert!(matches!(a, PopoverAction::Consumed));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        assert_eq!(
            q.audio.unwrap().volume,
            before,
            "arrow click must not move the volume"
        );
        let a = q.pointer_click(center(
            slider_arrow_rect(Slider::Output, q.layout()).unwrap(),
        ));
        assert!(matches!(a, PopoverAction::Consumed));
        assert!(q.expanded.is_none());
    }

    /// A sink hot-plugging mid-drag must not move the volume: the slider track's geometry is frozen
    /// at drag start, so the picker arrow can't appear, shrink the track, and remap `volume_from_x`
    /// (which would snap the handle under a stationary pointer). The frozen geometry lifts on
    /// release, and `end_drag` reports that the arrow now needs drawing.
    #[test]
    fn a_sink_hotplug_mid_drag_does_not_snap_the_volume() {
        let mut q = qs_with_sinks(1); // one sink → no arrow, full-width track
                                      // Press mid-track and hold.
        let track = slider_track_rect(Slider::Output, q.layout());
        let press = Point::from((
            track.loc.x + track.size.w * 0.5,
            track.loc.y + track.size.h / 2.,
        ));
        q.pointer_click(press);
        assert!(q.sliding.is_some());
        let held = q.audio.unwrap().volume;

        // A second sink appears while the button is held: the list updates, but the drag geometry
        // stays frozen (no arrow, track unchanged).
        assert!(q.set_sink_list(make_sinks(2)));
        assert!(
            slider_arrow_rect(Slider::Output, q.layout()).is_none(),
            "the picker arrow must stay suppressed mid-drag"
        );

        // The pointer hasn't moved → the volume must not have moved either.
        let dragged = q.pointer_drag(press).unwrap();
        assert!(matches!(dragged, PopoverAction::SetVolume(_)));
        assert_eq!(
            q.audio.unwrap().volume,
            held,
            "a stationary pointer must not snap the volume"
        );

        // Releasing lifts the freeze; the now-visible arrow needs a redraw, and its geometry is
        // live.
        assert!(
            q.end_drag(),
            "the mid-drag hotplug needs a redraw on release"
        );
        assert!(slider_arrow_rect(Slider::Output, q.layout()).is_some());
    }

    /// The picker lists one row per sink (default checked) then a "Sound Settings" row; a sink row
    /// sets that sink default, the settings row spawns.
    #[test]
    fn output_picker_lists_sinks_and_routes_actions() {
        let mut q = qs_with_sinks(3); // default = sink0
        q.pointer_click(center(
            slider_arrow_rect(Slider::Output, q.layout()).unwrap(),
        ));
        let rows = DetailOwner::Output.rows(q.network, &q.sink_list, &q.source_list, &q.power);
        assert_eq!(rows.len(), 4, "3 sinks + Sound Settings");
        assert!(rows[0].selected && !rows[1].selected && !rows[2].selected);
        assert!(rows[3].separator_before && !rows[3].selected);
        assert_eq!(rows[3].label, "Sound Settings");
        // Row geometry agrees with the shape.
        assert_eq!(
            DetailOwner::Output.row_shape(q.sink_list.sinks.len()).len(),
            rows.len()
        );

        match q.pointer_click(center(detail_row_rect(1, q.layout()).unwrap())) {
            PopoverAction::SetDefaultSink(name) => assert_eq!(name, "sink1"),
            other => panic!("expected SetDefaultSink, got {other:?}"),
        }
        match q.pointer_click(center(detail_row_rect(3, q.layout()).unwrap())) {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-control-center", "sound"]),
            other => panic!("expected the sound-settings spawn, got {other:?}"),
        }
    }

    /// The sink list is capped so the trailing settings row stays on-screen; the shape and rows
    /// agree under the cap.
    #[test]
    fn output_picker_caps_the_sink_rows() {
        let q = qs_with_sinks(MAX_DEVICE_ROWS + 3);
        let rows = DetailOwner::Output.rows(q.network, &q.sink_list, &q.source_list, &q.power);
        assert_eq!(
            rows.len(),
            MAX_DEVICE_ROWS + 1,
            "capped sinks + settings row"
        );
        assert_eq!(
            DetailOwner::Output.row_shape(q.sink_list.sinks.len()).len(),
            rows.len()
        );
    }

    /// The picker pins below the slider row (the slider itself doesn't move) and grows the menu;
    /// the grid shifts down.
    #[test]
    fn output_detail_anchors_below_the_slider() {
        let collapsed = qs_with_sinks(2).layout();
        let mut open = qs_with_sinks(2);
        open.expanded = Some(DetailOwner::Output);
        let expanded = open.layout();

        assert!(menu_h(expanded) > menu_h(collapsed), "the menu must grow");
        assert_eq!(
            slider_row_rect(Slider::Output, expanded).loc.y,
            slider_row_rect(Slider::Output, collapsed).loc.y,
            "the slider row (owner) must not shift"
        );
        let card = detail_rect(expanded).unwrap();
        let slider = slider_row_rect(Slider::Output, expanded);
        assert!(
            card.loc.y >= slider.loc.y + SLIDER_H - 0.01,
            "card sits below the slider"
        );
        assert!(
            tile_rect(0, expanded).loc.y > tile_rect(0, collapsed).loc.y,
            "the grid must shift down"
        );
    }

    /// The picker collapses if its owner disappears while open: sinks dropping to one (no arrow to
    /// close it) or the bound sink unbinding (the slider row itself vanishing).
    #[test]
    fn output_picker_collapses_when_its_owner_vanishes() {
        let mut q = qs_with_sinks(2);
        q.pointer_click(center(
            slider_arrow_rect(Slider::Output, q.layout()).unwrap(),
        ));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        assert!(q.set_sink_list(make_sinks(1)));
        assert!(q.expanded.is_none(), "one sink left → no picker");

        let mut q = qs_with_sinks(2);
        q.pointer_click(center(
            slider_arrow_rect(Slider::Output, q.layout()).unwrap(),
        ));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        assert!(q.set_audio(None));
        assert!(q.expanded.is_none(), "slider vanished → no picker");
    }
}
