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

use crate::audio::{AudioStatus, SinkList};
use crate::gnome::QuickToggles;
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::system_status::{self, BatteryStatus, NetworkStatus};
use crate::ui::popover::PopoverAction;
use crate::utils::to_physical_precise_round;

// Geometry, logical px (grounded in gnome-shell-sass quick-settings proportions).
const PAD: f64 = 12.;
const TILE_W: f64 = 150.;
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
    /// The volume slider's output-device picker (gnome-shell's `OutputStreamSlider` device menu).
    Output,
}

/// The most output sink rows the picker renders (Fable): the card grows with the sink count and the
/// popover has no scrolling, so cap the list to keep the trailing "Sound Settings" row on-screen.
/// Beyond this the extra sinks are dropped (a rare config — many null-sinks or a big HDMI/BT
/// fleet).
const MAX_SINK_ROWS: usize = 6;

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
            // gnome-shell's output slider header (`volume.js:314`).
            DetailOwner::Output => (
                vec!["audio-headphones-symbolic".to_string()],
                "Sound Output".to_string(),
            ),
        }
    }

    /// The action rows, top to bottom, given the live state.
    fn rows(self, network: NetworkStatus, sink_list: &SinkList) -> Vec<DetailRow> {
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
            // gnome-shell's output device list: one row per sink (label = description; the current
            // default carries a trailing check; clicking sets it default via a metadata write),
            // then a separator + a "Sound Settings" entry point (`volume.js:80-82,126-165`).
            DetailOwner::Output => {
                let mut rows: Vec<DetailRow> = sink_list
                    .sinks
                    .iter()
                    .take(MAX_SINK_ROWS)
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
        }
    }

    /// The per-row `separator_before` flags, top to bottom — the card's row *shape*, derived purely
    /// from the sink count (no label/state), so the geometry can size the card without building
    /// rows. MUST match `rows()`'s length + separators (a debug_assert checks it at the draw/hit
    /// sites). `sink_count` is ignored by the fixed owners.
    fn row_shape(self, sink_count: usize) -> Vec<bool> {
        match self {
            DetailOwner::Network => vec![false],
            DetailOwner::Power => vec![false, false, false, true],
            DetailOwner::Output => {
                let mut shape = vec![false; sink_count.min(MAX_SINK_ROWS)];
                shape.push(true); // the "Sound Settings" row, past a separator
                shape
            }
        }
    }

    /// The card's logical height: top pad + header + gap + rows + separators + bottom pad.
    fn detail_height(self, sink_count: usize) -> f64 {
        let shape = self.row_shape(sink_count);
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
    fn anchor_row_bottom(self, has_slider: bool) -> f64 {
        match self {
            DetailOwner::Network => {
                let row = (network_index() / COLS) as f64;
                grid_top(has_slider) + (row + 1.) * TILE_H + row * TILE_GAP
            }
            // The power button lives in the top system row, so its detail pins right below it —
            // above the slider and the whole grid, which shift down.
            DetailOwner::Power => PAD + SYS_H,
            // The output picker pins below the volume slider row (which only exists with a sink —
            // `normalize_expanded` guarantees Output is open only then).
            DetailOwner::Output => PAD + SYS_H + TILE_GAP + SLIDER_H,
        }
    }
}

/// The grid slot the Network tile occupies (its detail view anchors below this row). Derived by
/// identity so a `GRID` reorder can't desync the detail geometry.
fn network_index() -> usize {
    GRID.iter()
        .position(|t| matches!(t, GridTile::Network))
        .unwrap_or(0)
}

/// The menu-local layout context: everything the pure geometry functions need to place elements,
/// including the vertical shift a below-the-owner-row detail view imposes. Threaded through every
/// geometry fn so hit-testing and rendering share one source of truth for the shift — and, via
/// `sink_count`, size a dynamic detail view (the output picker's per-sink rows) identically to what
/// the renderer draws.
#[derive(Debug, Clone, Copy)]
struct Layout {
    has_slider: bool,
    expanded: Option<DetailOwner>,
    /// Number of output sinks — the only state a detail card's *size* depends on (the Output
    /// picker's row count). Fixed owners ignore it.
    sink_count: usize,
}

impl Layout {
    /// The detail card's `(natural insert y, block height)` when a view is open. The block height
    /// is the card plus its top margin — exactly how far the rows below the owner shift down, and
    /// how much taller the menu grows.
    fn detail_block(self) -> Option<(f64, f64)> {
        let owner = self.expanded?;
        Some((
            owner.anchor_row_bottom(self.has_slider),
            DETAIL_MARGIN + owner.detail_height(self.sink_count),
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
}

/// Grid order, row-major over two columns: Network leads in the prominent top-left
/// cell (gnome-shell's quick-settings grid leads with connectivity), then the
/// gsettings toggles fill out the 2×2 grid.
const GRID: [GridTile; 4] = [
    GridTile::Network,
    GridTile::Toggle(Tile::DarkStyle),
    GridTile::Toggle(Tile::DoNotDisturb),
    GridTile::Toggle(Tile::NightLight),
];

impl GridTile {
    /// The tile's label given the live toggle + network state.
    fn label(self, network: NetworkStatus) -> String {
        match self {
            GridTile::Toggle(t) => t.label().to_string(),
            GridTile::Network => network_label(network).to_string(),
        }
    }

    /// Candidate symbolic icon names, first that resolves wins.
    fn icons(self, network: NetworkStatus) -> Vec<String> {
        match self {
            GridTile::Toggle(t) => t.icons().iter().map(|s| s.to_string()).collect(),
            GridTile::Network => network_icons(network),
        }
    }

    /// Whether the tile reads as "on" (accent background): a toggle's gsettings
    /// state, or — for Network — whether a connection is currently up.
    fn is_on(self, toggles: QuickToggles, network: NetworkStatus) -> bool {
        match self {
            GridTile::Toggle(t) => t.is_on(toggles),
            GridTile::Network => {
                matches!(network, NetworkStatus::Wired | NetworkStatus::Wireless(_))
            }
        }
    }

    /// Whether this tile carries an expand-arrow that opens a detail view (gnome-shell's
    /// `QuickMenuToggle`). Only Network in v1; the gsettings toggles are plain [`QuickToggle`]s.
    fn detail_owner(self) -> Option<DetailOwner> {
        match self {
            GridTile::Network => Some(DetailOwner::Network),
            GridTile::Toggle(_) => None,
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
        NetworkStatus::Airplane => "Airplane Mode",
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
    /// The battery, for the far-left power pill; `None` hides it (desktop / VM
    /// without a battery), like gnome-shell's `PowerToggle.visible = IsPresent`.
    battery: Option<BatteryStatus>,
    /// Accent color for an active tile's background (straight RGBA).
    accent: [f32; 4],
    /// Default-sink state for the volume slider; `None` hides the slider row.
    audio: Option<AudioStatus>,
    /// The output sinks + current default, for the slider's device picker (empty → no picker
    /// arrow).
    sink_list: SinkList,
    /// Whether the volume slider is being dragged (a button is held on it).
    sliding: bool,
    /// The sink count frozen at drag start, so the slider track's geometry can't shift mid-drag if
    /// a sink hot-plugs (which would otherwise make the picker arrow appear/vanish, resize the
    /// track, and remap `volume_from_x` — snapping the volume under a stationary pointer).
    /// `Some` only while `sliding`; [`layout`](Self::layout) reads it in place of the live
    /// count.
    slide_sink_count: Option<usize>,
    /// Which tile's detail view is open (gnome-shell's single open `QuickToggleMenu`), or `None`
    /// when collapsed. At most one at a time.
    expanded: Option<DetailOwner>,
    /// Bumped on any toggle so the cached chrome texture is redrawn.
    revision: u64,
    cache: RefCell<TextureCache>,
}

struct TextureCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl QuickSettings {
    /// Open with the current toggle states, network state, and battery; `accent`
    /// straight RGB (e.g. `gnome_settings.accent_color`).
    pub fn new(
        toggles: QuickToggles,
        network: NetworkStatus,
        battery: Option<BatteryStatus>,
        audio: Option<AudioStatus>,
        sink_list: SinkList,
        accent: [u8; 3],
    ) -> Self {
        Self {
            toggles,
            network,
            battery,
            audio,
            sink_list,
            sliding: false,
            slide_sink_count: None,
            expanded: None,
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

    /// The current layout context (slider presence + which detail view is open + sink count), the
    /// single source of truth every geometry function shares.
    fn layout(&self) -> Layout {
        Layout {
            has_slider: self.audio.is_some(),
            expanded: self.expanded,
            sink_count: self.slide_sink_count.unwrap_or(self.sink_list.sinks.len()),
        }
    }

    /// Enforce the invariant that an open detail view's owner still exists: the Output picker is
    /// valid only while there's a slider (a bound sink) AND more than one sink to choose between —
    /// the same `>1` gate that shows its arrow. If a sink is removed (down to one) or the default
    /// unbinds while the picker is open, collapse it, so the card can't pin below a vanished slider
    /// row (broken geometry) or strand the user with an open card and no arrow to close it. Returns
    /// whether it collapsed (→ redraw). Network/Power owners always exist, so they're untouched.
    fn normalize_expanded(&mut self) -> bool {
        if self.expanded == Some(DetailOwner::Output)
            && (self.audio.is_none() || self.sink_list.sinks.len() <= 1)
        {
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
                .rows(self.network, &self.sink_list)
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
        for (i, item) in GRID.iter().enumerate() {
            // A menu tile's arrow-half toggles its detail view (open, or close if already open —
            // one at a time); the toggle-body keeps the tile's own behavior. A plain tile is all
            // body.
            if tile_arrow_rect(i, layout).is_some_and(|r| r.contains(pos)) {
                let owner = item.detail_owner();
                self.expanded = if self.expanded == owner { None } else { owner };
                self.revision += 1;
                return PopoverAction::Consumed;
            }
            if tile_body_rect(i, layout).contains(pos) {
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
                        self.set_tile(*tile, on);
                        self.revision += 1;
                        tile.action(on)
                    }
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
        // The volume slider: the speaker icon toggles mute; the arrow (when >1 sink) toggles the
        // output-device picker; the track jumps to (and begins dragging toward) the clicked
        // position. The arrow is tested BEFORE the track so an arrow click never starts a drag (the
        // track is genuinely shortened to make room, so `volume_from_x` stays in range).
        if self.audio.is_some() {
            if slider_icon_rect(layout).contains(pos) {
                return PopoverAction::ToggleMute;
            }
            if slider_arrow_rect(layout).is_some_and(|r| r.contains(pos)) {
                self.expanded = if self.expanded == Some(DetailOwner::Output) {
                    None
                } else {
                    Some(DetailOwner::Output)
                };
                self.revision += 1;
                return PopoverAction::Consumed;
            }
            if slider_track_rect(layout).contains(pos) {
                self.sliding = true;
                self.slide_sink_count = Some(self.sink_list.sinks.len());
                return self.set_local_volume(volume_from_x(pos.x, layout));
            }
        }
        PopoverAction::Consumed
    }

    /// Continue a slider drag: while a button is held on the track, motion updates
    /// the volume. Returns the action to apply, or `None` when not dragging.
    pub fn pointer_drag(&mut self, pos: Point<f64, Logical>) -> Option<PopoverAction> {
        let layout = self.layout();
        self.sliding
            .then(|| self.set_local_volume(volume_from_x(pos.x, layout)))
    }

    /// End a slider drag (pointer released). Returns whether releasing the frozen sink count
    /// changed the effective geometry (a sink hot-plugged mid-drag) — the caller redraws so the
    /// picker arrow that was suppressed during the drag now appears.
    pub fn end_drag(&mut self) -> bool {
        self.sliding = false;
        let unfrozen = self.slide_sink_count.take();
        unfrozen.is_some_and(|frozen| frozen != self.sink_list.sinks.len())
    }

    /// Optimistically move the slider locally (so the handle tracks the pointer
    /// before the PipeWire echo lands) and emit the volume action.
    fn set_local_volume(&mut self, volume: f64) -> PopoverAction {
        if let Some(audio) = self.audio.as_mut() {
            audio.volume = volume;
        }
        self.revision += 1;
        PopoverAction::SetVolume(volume)
    }

    /// Adopt a fresh sink state (from the PipeWire watcher). Ignored mid-drag so the
    /// lagging echo doesn't yank the handle. Returns whether it changed.
    pub fn set_audio(&mut self, audio: Option<AudioStatus>) -> bool {
        if self.sliding || audio == self.audio {
            return false;
        }
        self.audio = audio;
        self.revision += 1;
        // The slider vanishing (no bound sink) must also close an open output picker, or its card
        // pins below a slider row that's no longer there.
        self.normalize_expanded();
        true
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
        for (i, item) in GRID.iter().enumerate() {
            let on = item.is_on(self.toggles, self.network);
            let color = if on { FG_ON } else { FG_OFF };
            let rect = tile_rect(i, layout);
            let center = Point::from((
                rect.loc.x + TILE_ICON_INSET + TILE_ICON / 2.,
                rect.loc.y + rect.size.h / 2.,
            ));
            let candidates = item.icons(self.network);
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
            if let Some(arrow) = tile_arrow_rect(i, layout) {
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
                    .rows(self.network, &self.sink_list)
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

        // The volume slider's mute/level speaker icon, in its disc.
        if let Some(audio) = self.audio {
            let disc = slider_icon_rect(layout);
            let center =
                Point::from((disc.loc.x + disc.size.w / 2., disc.loc.y + disc.size.h / 2.));
            let name = crate::audio::volume_icon(&audio).to_string();
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
            // The device-picker arrow at the right of the slider row (when >1 sink).
            if let Some(arrow) = slider_arrow_rect(layout) {
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
        let labels: Vec<String> = GRID.iter().map(|item| item.label(self.network)).collect();
        // `.quick-toggle-title` / the power toggle's percentage are %heading = weight 700.
        let label_runs: Vec<_> = labels
            .iter()
            .map(|l| renderer.build_glyph_run_weighted(l, label_px, true))
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
                let rows = owner.rows(self.network, &self.sink_list);
                // The card is sized from the pure `row_shape`; assert the live rows match it (count
                // + separator positions) so the geometry can't drift from what's drawn here.
                debug_assert_eq!(
                    rows.iter().map(|r| r.separator_before).collect::<Vec<_>>(),
                    owner.row_shape(self.sink_list.sinks.len()),
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

            for (i, item) in GRID.iter().enumerate() {
                let rect = tile_rect(i, layout);
                let on = item.is_on(self.toggles, self.network);
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

                // A menu tile's arrow-half is separated from the body by a 1px divider
                // (`.quick-toggle-separator`); v1 keeps one pill background and marks the split
                // with the divider + the arrow icon (the split-radius look is a later cosmetic).
                if let Some(arrow) = tile_arrow_rect(i, layout) {
                    let sep = Rectangle::new(
                        Point::from((arrow.loc.x - SEPARATOR_W, arrow.loc.y + arrow.size.h * 0.2)),
                        Size::from((SEPARATOR_W, arrow.size.h * 0.6)),
                    );
                    frame.render_rounded_rect(SEPARATOR_COLOR, 0., rect_px(sep), &[full])?;
                }

                let fg = if on { FG_ON } else { FG_OFF };
                let label_x = px(rect.loc.x + TILE_ICON_INSET + TILE_ICON + 8.);
                let label_cy = px(rect.loc.y + rect.size.h / 2.);
                let run = &label_runs[i];
                // Clip the label to the toggle-body so a long name can't run under the arrow
                // (gnome-shell ellipsizes; clipping is the minimal faithful bound).
                let clip = rect_px(tile_body_rect(i, layout));
                frame.render_glyphs(
                    run,
                    place_left(run.ink_bounds(), label_x, label_cy),
                    fg,
                    clip,
                    &[full],
                )?;
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
            }

            // The volume slider: mute-button disc, the trough, its accent-filled
            // portion, and the round handle (`.quick-slider` + `_slider.scss`). The
            // speaker icon composites on top afterwards.
            if let Some(audio) = self.audio {
                let disc = slider_icon_rect(layout);
                frame.render_rounded_rect(
                    TILE_OFF,
                    (SLIDER_H / 2. * scale) as f32,
                    rect_px(disc),
                    &[full],
                )?;

                let track = slider_track_rect(layout);
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

                let handle_cx = slider_handle_x(audio.volume, layout);
                let fill_color = if audio.muted {
                    SLIDER_TROUGH_BG
                } else {
                    self.accent
                };
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
                    let has_icon = self
                        .expanded
                        .map(|o| o.rows(self.network, &self.sink_list))
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

/// The y of the tile grid's top edge. The grid sits below the system row, and below
/// the volume slider too when a sink is present (gnome-shell orders the output slider
/// right under the system item, above the toggle tiles).
fn grid_top(has_slider: bool) -> f64 {
    let mut y = PAD + SYS_H + TILE_GAP;
    if has_slider {
        y += SLIDER_H + TILE_GAP;
    }
    y
}

/// The y of the grid's bottom edge (grid top + tile rows), before any detail shift.
fn grid_bottom(has_slider: bool) -> f64 {
    let rows = GRID.len().div_ceil(COLS) as f64;
    grid_top(has_slider) + rows * TILE_H + (rows - 1.) * TILE_GAP
}

/// The menu's logical height: system row, optional volume slider, tile grid, padding — grown by
/// the open detail view's block (the card plus its top margin) when one is expanded.
fn menu_h(layout: Layout) -> f64 {
    let base = grid_bottom(layout.has_slider) + PAD;
    base + layout.detail_block().map(|(_, h)| h).unwrap_or(0.)
}

/// The volume-slider row rectangle (full content width), between the system row and
/// the tile grid. Shifts down under a detail view whose owner sits above it.
fn slider_row_rect(layout: Layout) -> Rectangle<f64, Logical> {
    let y = PAD + SYS_H + TILE_GAP;
    Rectangle::new(
        Point::from((PAD, y + layout.shift_below(y))),
        Size::from((menu_w() - 2. * PAD, SLIDER_H)),
    )
}

/// The mute-button disc at the left of the slider row.
fn slider_icon_rect(layout: Layout) -> Rectangle<f64, Logical> {
    let row = slider_row_rect(layout);
    Rectangle::new(row.loc, Size::from((SLIDER_H, SLIDER_H)))
}

/// The output-device picker's menu-button (`go-next` arrow) at the right end of the slider row —
/// gnome-shell's `QuickSlider._menuButton`, shown **only when there's more than one sink** to pick
/// between (`menuEnabled = _deviceItems.size > 1`). `None` otherwise.
fn slider_arrow_rect(layout: Layout) -> Option<Rectangle<f64, Logical>> {
    if layout.sink_count <= 1 {
        return None;
    }
    let row = slider_row_rect(layout);
    Some(Rectangle::new(
        Point::from((row.loc.x + row.size.w - ARROW_W, row.loc.y)),
        Size::from((ARROW_W, SLIDER_H)),
    ))
}

/// The slider's interactive track band (row minus the mute disc, and minus the device-picker arrow
/// when shown). The drawn trough is a thin bar centered in this band; the usable handle-center
/// x-range is inset by half a handle so the handle never overhangs. Genuinely shortened when the
/// arrow is present, so an arrow click never lands on the track and `volume_from_x` stays in range.
fn slider_track_rect(layout: Layout) -> Rectangle<f64, Logical> {
    let row = slider_row_rect(layout);
    let x = row.loc.x + SLIDER_H + SYS_GAP;
    let right = match slider_arrow_rect(layout) {
        Some(arrow) => arrow.loc.x - SYS_GAP,
        None => row.loc.x + row.size.w,
    };
    Rectangle::new(
        Point::from((x, row.loc.y)),
        Size::from((right - x, SLIDER_H)),
    )
}

/// Perceptual volume `0..=1` for a pointer x on the slider track.
fn volume_from_x(x: f64, layout: Layout) -> f64 {
    let track = slider_track_rect(layout);
    let left = track.loc.x + SLIDER_HANDLE / 2.;
    let right = track.loc.x + track.size.w - SLIDER_HANDLE / 2.;
    ((x - left) / (right - left)).clamp(0.0, 1.0)
}

/// The x of the handle center for a given perceptual volume.
fn slider_handle_x(volume: f64, layout: Layout) -> f64 {
    let track = slider_track_rect(layout);
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
    let y = grid_top(layout.has_slider) + row * (TILE_H + TILE_GAP);
    Rectangle::new(
        Point::from((x, y + layout.shift_below(y))),
        Size::from((TILE_W, TILE_H)),
    )
}

/// The expand-arrow (menu-button) half of a menu-bearing tile — the right `ARROW_W`, full height —
/// or `None` for a plain toggle. gnome-shell's `.quick-toggle-menu-button`, the second hit region.
fn tile_arrow_rect(i: usize, layout: Layout) -> Option<Rectangle<f64, Logical>> {
    GRID[i].detail_owner()?;
    let tile = tile_rect(i, layout);
    Some(Rectangle::new(
        Point::from((tile.loc.x + tile.size.w - ARROW_W, tile.loc.y)),
        Size::from((ARROW_W, tile.size.h)),
    ))
}

/// The toggle-body half of a tile (the whole tile for a plain toggle; the tile minus the arrow
/// for a menu tile). Its click flips the toggle / opens settings; also the label's clip bound.
fn tile_body_rect(i: usize, layout: Layout) -> Rectangle<f64, Logical> {
    let tile = tile_rect(i, layout);
    match tile_arrow_rect(i, layout) {
        Some(_) => Rectangle::new(
            tile.loc,
            Size::from((tile.size.w - ARROW_W - SEPARATOR_W, tile.size.h)),
        ),
        None => tile,
    }
}

/// The detail-view card rectangle (menu-local logical), pinned below its owner's row, or `None`
/// when collapsed.
fn detail_rect(layout: Layout) -> Option<Rectangle<f64, Logical>> {
    let owner = layout.expanded?;
    let insert_y = owner.anchor_row_bottom(layout.has_slider);
    Some(Rectangle::new(
        Point::from((PAD, insert_y + DETAIL_MARGIN)),
        Size::from((menu_w() - 2. * PAD, owner.detail_height(layout.sink_count))),
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
    let shape = owner.row_shape(layout.sink_count);
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

    fn battery(percentage: f64) -> BatteryStatus {
        BatteryStatus {
            icon_name: "battery-level-80-symbolic".to_string(),
            percentage,
        }
    }

    fn center(r: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
    }

    /// A collapsed layout (no open detail view) with the given slider presence.
    fn lay(has_slider: bool) -> Layout {
        Layout {
            has_slider,
            expanded: None,
            sink_count: 0,
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
                None,
                audio,
                SinkList::default(),
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
            for i in 0..GRID.len() {
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
                within(slider_row_rect(lay(has_slider)), "slider row");
                within(slider_icon_rect(lay(has_slider)), "slider mute button");
                within(slider_track_rect(lay(has_slider)), "slider track");
                // The slider sits above the tile grid.
                assert!(
                    slider_row_rect(lay(has_slider)).loc.y
                        + slider_row_rect(lay(has_slider)).size.h
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
            None,
            None,
            SinkList::default(),
            [0, 0, 0],
        );
        let dnd = tile_rect(2, lay(false)); // grid: [Network, Dark Style, Do Not Disturb, Night Light]
        let before = qs.revision;
        let action = qs.pointer_click(center(dnd));
        assert!(matches!(action, PopoverAction::SetDoNotDisturb(true)));
        assert!(qs.toggles.do_not_disturb);
        assert!(qs.revision > before);
    }

    /// The Network tile (grid cell 0) reads as "on" when connected and, clicked,
    /// opens network settings without flipping any local toggle state.
    #[test]
    fn network_tile_reflects_state_and_opens_settings() {
        assert!(GridTile::Network.is_on(QuickToggles::default(), NetworkStatus::Wired));
        assert!(GridTile::Network.is_on(QuickToggles::default(), NetworkStatus::Wireless(60)));
        assert!(!GridTile::Network.is_on(QuickToggles::default(), NetworkStatus::Offline));
        assert!(!GridTile::Network.is_on(QuickToggles::default(), NetworkStatus::Airplane));

        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            None,
            None,
            SinkList::default(),
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
            None,
            None,
            SinkList::default(),
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
            Some(battery(79.)),
            None,
            SinkList::default(),
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
            None,
            None,
            SinkList::default(),
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
            Some(battery(79.)),
            None,
            SinkList::default(),
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
            None,
            audio,
            SinkList::default(),
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

    /// A menu tile's arrow-half and toggle-body are disjoint hit regions; a plain toggle has no
    /// arrow and its body is the whole tile.
    #[test]
    fn tile_body_and_arrow_are_disjoint_regions() {
        let l = lay(false);
        let ni = network_index();
        let body = tile_body_rect(ni, l);
        let arrow = tile_arrow_rect(ni, l).expect("the Network tile carries an arrow");
        // The body ends at (or before) the arrow's left edge — a separator sits between.
        assert!(body.loc.x + body.size.w <= arrow.loc.x);
        assert_eq!(arrow.loc.x + arrow.size.w, tile_rect(ni, l).loc.x + TILE_W);
        // A gsettings toggle (Do Not Disturb, cell 2) is all body, no arrow.
        assert!(tile_arrow_rect(2, l).is_none());
        assert_eq!(tile_body_rect(2, l), tile_rect(2, l));
    }

    /// The Network arrow opens the detail view; clicking it again collapses it. Both are internal
    /// state changes (Consumed) that bump the chrome revision.
    #[test]
    fn network_arrow_toggles_the_detail_view() {
        let mut qs = qs(NetworkStatus::Wired, None);
        let ni = network_index();
        assert!(qs.expanded.is_none());

        let before = qs.revision;
        let a = qs.pointer_click(center(tile_arrow_rect(ni, qs.layout()).unwrap()));
        assert!(matches!(a, PopoverAction::Consumed));
        assert_eq!(qs.expanded, Some(DetailOwner::Network));
        assert!(qs.revision > before);

        // Network is a row-0 tile, so opening its detail doesn't shift it — the arrow stays put.
        let a = qs.pointer_click(center(tile_arrow_rect(ni, qs.layout()).unwrap()));
        assert!(matches!(a, PopoverAction::Consumed));
        assert!(qs.expanded.is_none());
    }

    /// Opening a detail view grows the menu (taller, same width) and shifts only the rows below
    /// the owner's row; the card sits between the owner's row and the shifted rows.
    #[test]
    fn detail_view_grows_menu_and_shifts_lower_rows_only() {
        let collapsed = Layout {
            has_slider: false,
            expanded: None,
            sink_count: 0,
        };
        let expanded = Layout {
            has_slider: false,
            expanded: Some(DetailOwner::Network),
            sink_count: 0,
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
        for i in COLS..GRID.len() {
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
            tile_arrow_rect(network_index(), qs.layout()).unwrap(),
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
            tile_arrow_rect(network_index(), qs.layout()).unwrap(),
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
                    has_slider,
                    expanded: Some(owner),
                    sink_count: 0,
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
        let rows = DetailOwner::Power.rows(NetworkStatus::Unknown, &SinkList::default());
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
            tile_arrow_rect(network_index(), qs.layout()).unwrap(),
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
            has_slider: true,
            expanded: None,
            sink_count: 0,
        };
        let expanded = Layout {
            has_slider: true,
            expanded: Some(DetailOwner::Power),
            sink_count: 0,
        };
        let block = DETAIL_MARGIN + DetailOwner::Power.detail_height(0);
        assert_eq!(
            sys_rect(SysButton::Power, false).loc.y,
            PAD,
            "system row must not move"
        );
        let ds = slider_row_rect(expanded).loc.y - slider_row_rect(collapsed).loc.y;
        assert!(
            (ds - block).abs() < 0.01,
            "the slider must shift by the block, got {ds}"
        );
        for i in 0..GRID.len() {
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
                slider_arrow_rect(qs_with_sinks(n).layout()).is_some(),
                expect,
                "n={n}"
            );
        }
        let one = qs_with_sinks(1);
        let two = qs_with_sinks(2);
        let t1 = slider_track_rect(one.layout());
        let t2 = slider_track_rect(two.layout());
        assert!(
            t2.loc.x + t2.size.w < t1.loc.x + t1.size.w,
            "the track must shrink to make room for the arrow"
        );
        // volume_from_x still maps the full 0..1 within the shortened track.
        let l = two.layout();
        let track = slider_track_rect(l);
        assert!(volume_from_x(track.loc.x, l).abs() < 1e-9);
        assert!((volume_from_x(track.loc.x + track.size.w, l) - 1.0).abs() < 1e-9);
    }

    /// The slider arrow toggles the output picker and never moves the volume; the slider row (its
    /// owner) doesn't shift, so the arrow stays hittable to close it.
    #[test]
    fn slider_arrow_toggles_the_picker_without_changing_volume() {
        let mut q = qs_with_sinks(2);
        let before = q.audio.unwrap().volume;
        let a = q.pointer_click(center(slider_arrow_rect(q.layout()).unwrap()));
        assert!(matches!(a, PopoverAction::Consumed));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        assert_eq!(
            q.audio.unwrap().volume,
            before,
            "arrow click must not move the volume"
        );
        let a = q.pointer_click(center(slider_arrow_rect(q.layout()).unwrap()));
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
        let track = slider_track_rect(q.layout());
        let press = Point::from((
            track.loc.x + track.size.w * 0.5,
            track.loc.y + track.size.h / 2.,
        ));
        q.pointer_click(press);
        assert!(q.sliding);
        let held = q.audio.unwrap().volume;

        // A second sink appears while the button is held: the list updates, but the drag geometry
        // stays frozen (no arrow, track unchanged).
        assert!(q.set_sink_list(make_sinks(2)));
        assert!(
            slider_arrow_rect(q.layout()).is_none(),
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
        assert!(slider_arrow_rect(q.layout()).is_some());
    }

    /// The picker lists one row per sink (default checked) then a "Sound Settings" row; a sink row
    /// sets that sink default, the settings row spawns.
    #[test]
    fn output_picker_lists_sinks_and_routes_actions() {
        let mut q = qs_with_sinks(3); // default = sink0
        q.pointer_click(center(slider_arrow_rect(q.layout()).unwrap()));
        let rows = DetailOwner::Output.rows(q.network, &q.sink_list);
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
        let q = qs_with_sinks(MAX_SINK_ROWS + 3);
        let rows = DetailOwner::Output.rows(q.network, &q.sink_list);
        assert_eq!(rows.len(), MAX_SINK_ROWS + 1, "capped sinks + settings row");
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
            slider_row_rect(expanded).loc.y,
            slider_row_rect(collapsed).loc.y,
            "the slider row (owner) must not shift"
        );
        let card = detail_rect(expanded).unwrap();
        let slider = slider_row_rect(expanded);
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
        q.pointer_click(center(slider_arrow_rect(q.layout()).unwrap()));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        assert!(q.set_sink_list(make_sinks(1)));
        assert!(q.expanded.is_none(), "one sink left → no picker");

        let mut q = qs_with_sinks(2);
        q.pointer_click(center(slider_arrow_rect(q.layout()).unwrap()));
        assert_eq!(q.expanded, Some(DetailOwner::Output));
        assert!(q.set_audio(None));
        assert!(q.expanded.is_none(), "slider vanished → no picker");
    }
}
