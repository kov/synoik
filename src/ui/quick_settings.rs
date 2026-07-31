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
//! Deferred vs gnome-shell: the brightness slider's per-monitor detail card, the
//! Network tile's in-menu enable/disable + connection list (its detail card is just
//! a settings entry point), and SSID/connection-name labels. The self-contained
//! tiles, the Network status tile, the Bluetooth tile (device list included), the
//! system row, the battery pill, and the volume, mic and brightness sliders are here.

use std::cell::RefCell;
use std::collections::HashMap;

use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::audio::{AudioStatus, MicStatus, SinkList, SourceList};
use crate::gnome::QuickToggles;
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::system_status::{
    self, AirplaneStatus, BatteryStatus, BluetoothRfkill, BluetoothStatus, BtAdapterState,
    NetworkStatus, PowerProfileStatus,
};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{self, style, Align, Painter, ShapedText, TextShaper, TextStyle};
use crate::utils::to_physical_precise_round;

// Geometry, logical px (grounded in gnome-shell-sass quick-settings proportions).
/// `.quick-settings` padding is `$base_padding * 3` (`$base_padding: 6px` → 18px) — the uniform
/// outer margin every element insets from (`_quick-settings.scss:2`).
const PAD: f64 = 18.;
/// `.quick-toggle` is a fixed `12em` (`min/max-width`, `_quick-settings.scss:17-18`). `1em` is
/// `$base_font_size` (11pt), so 12em = `12 * pt_to_px(11)` ≈ 176px — wider than the old 150, which
/// cramped two-line tiles (Power Mode) and made the whole menu narrower than gnome-shell's.
fn tile_w() -> f64 {
    12.0 * crate::ui::pt_to_px(11.0)
}
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
const LABEL_PT: f64 = 11.;

/// A tile subtitle's font size — gnome-shell's `.quick-toggle-subtitle` is `%caption` (9pt),
/// regular weight (`gnome-shell-sass/widgets/_quick-settings.scss`). Only Power Mode uses a
/// subtitle so far.
const SUBTITLE_PT: f64 = 9.;

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

/// Inactive quick-toggle / pill / slider-trough fill — GNOME's `.button` normal
/// `mix($fg_color, $bg_color, 9%)` ≈ `#48484c` (the quick-toggle extends `.button`,
/// `_quick-settings.scss:5,15`). Shared [`widget::style::BUTTON_BG`], the raised control on the
/// menu bg. (The popup box bg itself is now drawn by the shared popover chrome.)
const TILE_OFF: [f32; 4] = widget::style::BUTTON_BG;
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
const DETAIL_HEADER_ICON: f64 = 24.; // `$medium_scalable_icon_size`
/// The `.header .icon` circular pill: the 24px icon plus `padding: 1.5 * $base_padding` (9px) a
/// side → a 42px disc, `border-radius: $forced_circular_radius`, filling the header row's height.
const DETAIL_HEADER_ICON_PAD: f64 = 9.;
const DETAIL_HEADER_PILL: f64 = DETAIL_HEADER_ICON + 2. * DETAIL_HEADER_ICON_PAD;
/// The `.header .icon` background — `transparentize($fg_color, 0.8)` (white @ 20%), the
/// highlighted pill behind the header icon; the accent `.active` variant only applies to a
/// checked toggle's menu, never the shutdown menu.
const DETAIL_HEADER_PILL_BG: [f32; 4] = [1., 1., 1., 0.2];
/// The header row is as tall as its icon pill (`$medium_scalable_icon_size` + padding).
const DETAIL_HEADER_H: f64 = DETAIL_HEADER_PILL;
const DETAIL_HEADER_INSET: f64 = 10.;
/// `.header` `padding-bottom` / `spacing-columns` = `$base_padding * 2` (12px): the gap below the
/// header before the rows, and between the icon pill and the title.
const DETAIL_HEADER_GAP: f64 = 12.;
const DETAIL_ROW_H: f64 = 36.;
const DETAIL_ROW_GAP: f64 = 2.;
const DETAIL_ROW_INSET: f64 = 12.;
/// Extra space above a row that follows a group separator (e.g. the machine-power vs session
/// split in the shutdown menu) — the `.popup-separator-menu-item`'s slot, with a 1px
/// `$borders_color` rule (shared [`widget::style::BORDERS`]) drawn centered in it.
const DETAIL_SEP_EXTRA: f64 = 8.;
/// Detail-card surface — gnome-shell's `%card` = `$card_bg_color` = `lighten($bg_color, 7%)`
/// ≈ `#47474c`, one step lighter than the menu box. Shared [`widget::style::CARD_BG`].
const CARD_BG: [f32; 4] = widget::style::CARD_BG;
/// Header-title font size, GNOME points (shaped via [`TextShaper`]). The header title is
/// `%title_3` (15pt/700); rows are regular-weight `.popup-menu-item` at `DETAIL_ROW_PT`.
const DETAIL_TITLE_PT: f64 = 15.;
const DETAIL_ROW_PT: f64 = 11.;
/// The bluetooth placeholder line: `.bt-menu-placeholder` extends `%title_4` = 13pt/700
/// (`_quick-settings.scss:227-232`, `_common.scss` heading scale).
const DETAIL_PLACEHOLDER_PT: f64 = 13.;
/// A row's trailing sublabel color: `.device-subtitle` = `transparentize($fg_color, 0.5)`
/// (`_quick-settings.scss:234`).
const DETAIL_SUBTITLE_FG: [f32; 4] = [1., 1., 1., 0.5];

/// A detail row's shaped text + the per-row flags the draw loop needs (so it never re-derives
/// rows mid-bake).
/// A baked card row: shaped text for the ordinary and label rows, a bare value for a slider one.
enum DetailRowRun {
    Text(TextRun),
    Slider { value: f64 },
}

struct TextRun {
    label: ShapedText,
    /// The trailing right-aligned sublabel run (Connect/Disconnect/busy), if any.
    trailing: Option<ShapedText>,
    placeholder: bool,
    has_icon: bool,
}

/// One actionable row in a detail view (gnome-shell's `addAction` items). `separator_before`
/// opens a visual group break above the row (the shutdown menu's power/session split).
struct ItemRow {
    label: String,
    /// Optional leading symbolic-icon candidates (empty = label-only, like the shutdown rows).
    icons: Vec<String>,
    action: PopoverAction,
    separator_before: bool,
    /// Whether this row is the current selection (a trailing check, gnome-shell's
    /// `Ornament.CHECK`).
    selected: bool,
    /// A trailing right-aligned sublabel (the bluetooth row's Connect/Disconnect,
    /// `.device-subtitle` = fg@50%, `bluetooth.js:242-246`). Mutually exclusive with `selected`.
    trailing: Option<String>,
    /// A non-reactive placeholder line (the bluetooth menu's `.bt-menu-placeholder`,
    /// `bluetooth.js:286-295`): drawn centered/bold, no hover, click consumed.
    placeholder: bool,
}

impl From<ItemRow> for DetailRow {
    fn from(row: ItemRow) -> Self {
        DetailRow::Item(row)
    }
}

/// One row of an open detail card.
///
/// Most cards are lists of actionable items, but the brightness card is a stack of
/// (name label, slider) pairs (`brightness.js:13-34`), so a row is not always a clickable label.
/// The variants are what the consumers genuinely branch on: only `Item` rows have an action, an
/// icon or a check; only `Slider` rows drag; and `Label`/`Slider` are both `reactive: false` in
/// gnome-shell (`brightness.js:14,29`), so neither ever highlights on hover.
enum DetailRow {
    Item(ItemRow),
    /// A non-reactive name label above a monitor's slider (`brightness.js:14-15`).
    Label(String),
    /// A bare slider bound to one output's scale (`brightness.js:17-31`) — no icon, unlike the
    /// top-level `QuickSlider` rows.
    Slider {
        connector: String,
        value: f64,
    },
}

/// What kind of row occupies a slot, and thus how tall it is. Part of the *pure* card shape, so
/// the geometry can size a card without building its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// An ordinary actionable list row.
    Item,
    /// A non-reactive text label.
    Label,
    /// A slider.
    Slider,
}

impl RowKind {
    /// All three are ordinary `%menuitem`s in gnome-shell (`brightness.js:14,29` builds the label
    /// and the slider bin as plain menu items), so they share a height today. The per-kind hook
    /// exists so a kind can diverge without another geometry refactor.
    fn height(self) -> f64 {
        DETAIL_ROW_H
    }
}

/// One slot of a card's pure shape: its kind, and whether a group separator opens above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowSpec {
    kind: RowKind,
    separator_before: bool,
}

impl RowSpec {
    /// The ordinary actionable row every card but brightness is made of.
    fn item(separator_before: bool) -> Self {
        Self {
            kind: RowKind::Item,
            separator_before,
        }
    }
}

impl DetailRow {
    /// The shape slot this row must occupy — what ties the live rows to the pure `row_shape` the
    /// geometry sized the card from.
    fn spec(&self) -> RowSpec {
        match self {
            DetailRow::Item(row) => RowSpec::item(row.separator_before),
            DetailRow::Label(_) => RowSpec {
                kind: RowKind::Label,
                separator_before: false,
            },
            DetailRow::Slider { .. } => RowSpec {
                kind: RowKind::Slider,
                separator_before: false,
            },
        }
    }

    /// The item behind an ordinary row; `None` for the label/slider rows, which have no action,
    /// icon or check.
    fn item(&self) -> Option<&ItemRow> {
        match self {
            DetailRow::Item(row) => Some(row),
            _ => None,
        }
    }
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
    /// The Bluetooth grid tile's device list (gnome-shell's `BluetoothToggle` menu).
    Bluetooth,
    /// The volume slider's output-device picker (gnome-shell's `OutputStreamSlider` device menu).
    Output,
    /// The mic slider's input-device picker (gnome-shell's `InputStreamSlider` device menu).
    Input,
    /// The brightness slider's per-monitor card (gnome-shell's `BrightnessSliderMenu`).
    Brightness,
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
    /// The display-brightness slider, stacked below the mic slider — gnome-shell adds the
    /// brightness item right after the two volume items (`panel.js:366-373`).
    Brightness,
}

/// The slider rows, top to bottom. Kept as an array so every draw/hit loop covers all of them.
const SLIDERS: [Slider; 3] = [Slider::Output, Slider::Mic, Slider::Brightness];

/// Which slider a drag is on. A card slider is per-connector, so it can't be named by the
/// top-level [`Slider`] enum — but [`Layout`] stays `Copy` by only ever carrying the top-level
/// half (a card slider has no arrow, so there is nothing for the drag freeze to pin).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SliderId {
    Top(Slider),
    /// A per-monitor slider row inside the brightness card, by connector.
    Monitor(String),
}

impl Slider {
    /// The detail picker this slider's arrow opens, if it has one.
    fn owner(self) -> Option<DetailOwner> {
        match self {
            Slider::Output => Some(DetailOwner::Output),
            Slider::Mic => Some(DetailOwner::Input),
            Slider::Brightness => Some(DetailOwner::Brightness),
        }
    }

    /// Whether the slider's leading icon is a button. gnome-shell's `QuickSlider` icon is
    /// **opt-in** reactive (`quickSettings.js:290-311`): the volume sliders opt in, to toggle
    /// mute; brightness does not, so its icon is decoration.
    fn icon_is_button(self) -> bool {
        match self {
            Slider::Output | Slider::Mic => true,
            Slider::Brightness => false,
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
    /// The brightness slider, shown iff the manager has a global scale — i.e. iff some monitor
    /// has a usable backlight (`brightness.js:59-60`).
    brightness: bool,
}

impl Sliders {
    /// Count of present slider rows (0..=3).
    fn count(self) -> usize {
        self.output as usize + self.mic as usize + self.brightness as usize
    }

    fn present(self, sl: Slider) -> bool {
        match sl {
            Slider::Output => self.output,
            Slider::Mic => self.mic,
            Slider::Brightness => self.brightness,
        }
    }

    /// This slider's vertical slot among the *present* sliders, top-down, in the order
    /// gnome-shell adds them: output, mic, brightness. Each one takes the next free slot, so a
    /// hidden slider above simply closes the gap.
    fn slot(self, sl: Slider) -> usize {
        match sl {
            Slider::Output => 0,
            Slider::Mic => self.output as usize,
            Slider::Brightness => self.output as usize + self.mic as usize,
        }
    }
}

/// A spawn `DetailRow` from a command's words.
fn spawn_row(label: &str, cmd: &[&str], separator_before: bool) -> DetailRow {
    ItemRow {
        label: label.to_string(),
        icons: Vec::new(),
        action: PopoverAction::Spawn(cmd.iter().map(|s| s.to_string()).collect()),
        separator_before,
        selected: false,
        trailing: None,
        placeholder: false,
    }
    .into()
}

/// The live Bluetooth state a detail card renders from: the snapshot, the row order frozen when
/// the card opened (gnome-shell reorders only at menu open, `bluetooth.js:326-331,384-393`), and
/// the device path with a connect/disconnect in flight (its busy mark).
/// Everything the audio device pickers resolve from. Bundled because the rows are a function of
/// all three together: a picker row is a card *port*, matched against the sink/source lists to find
/// the node it belongs to and whether it is the current one.
struct AudioDetail<'a> {
    sinks: &'a SinkList,
    sources: &'a SourceList,
    cards: &'a crate::audio::AudioCards,
}

struct BtDetail<'a> {
    status: &'a BluetoothStatus,
    order: &'a [String],
    busy: Option<&'a str>,
}

impl DetailOwner {
    /// The header shown at the top of the detail card: symbolic-icon candidates + title, given
    /// the live state the owner reflects.
    fn header(self, network: NetworkStatus) -> (Vec<String>, String) {
        match self {
            // gnome-shell's `menu.setHeader('bluetooth-active-symbolic', _('Bluetooth'))`
            // (`bluetooth.js:280`).
            DetailOwner::Bluetooth => (
                vec!["bluetooth-active-symbolic".to_string()],
                "Bluetooth".to_string(),
            ),
            DetailOwner::Network => (network_icons(network), network_label(network).to_string()),
            // `menu.setHeader('display-brightness-symbolic', _('Brightness'))`
            // (`brightness.js:47`).
            DetailOwner::Brightness => (
                vec!["display-brightness-symbolic".to_string()],
                "Brightness".to_string(),
            ),
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
        audio: AudioDetail<'_>,
        power: &PowerProfileStatus,
        bt: BtDetail<'_>,
        monitors: &[crate::brightness::MonitorView],
    ) -> Vec<DetailRow> {
        match self {
            // gnome-shell's device list (`bluetooth.js:283-304,395-408`): one row per visible
            // (connectable, paired‖trusted, adapter on) device — icon + alias + a trailing
            // Connect/Disconnect sublabel — in the order frozen at open with newcomers appended;
            // a placeholder when there are none; then a separator + "Bluetooth Settings".
            DetailOwner::Bluetooth => {
                let visible = bt.status.visible_devices();
                let mut ordered: Vec<&crate::system_status::BluetoothDevice> = bt
                    .order
                    .iter()
                    .filter_map(|p| visible.iter().find(|d| &d.path == p).copied())
                    .collect();
                for d in &visible {
                    if !bt.order.contains(&d.path) {
                        ordered.push(d);
                    }
                }
                let mut rows: Vec<DetailRow> = ordered
                    .into_iter()
                    .take(MAX_DEVICE_ROWS)
                    .map(|d| {
                        ItemRow {
                            label: d.alias.clone(),
                            icons: d.icon_candidates(),
                            action: PopoverAction::ConnectBluetoothDevice {
                                path: d.path.clone(),
                                connect: !d.connected,
                            },
                            separator_before: false,
                            selected: false,
                            // The busy mark stands in for gnome-shell's spinner (which hides the
                            // subtitle while a connect is in flight, `bluetooth.js:225-231`).
                            trailing: Some(if bt.busy == Some(d.path.as_str()) {
                                "…".to_string()
                            } else if d.connected {
                                "Disconnect".to_string()
                            } else {
                                "Connect".to_string()
                            }),
                            placeholder: false,
                        }
                        .into()
                    })
                    .collect();
                if rows.is_empty() {
                    // `.bt-menu-placeholder`, text by adapter state (`bluetooth.js:348-352`).
                    rows.push(
                        ItemRow {
                            label: if bt.status.powered {
                                "No available or connected devices"
                            } else {
                                "Turn on Bluetooth to connect to devices"
                            }
                            .to_string(),
                            icons: Vec::new(),
                            action: PopoverAction::Consumed,
                            separator_before: false,
                            selected: false,
                            trailing: None,
                            placeholder: true,
                        }
                        .into(),
                    );
                }
                rows.push(spawn_row(
                    "Bluetooth Settings",
                    &["gnome-control-center", "bluetooth"],
                    true,
                ));
                rows
            }
            // v1 Network detail: a single entry point to the full settings (the in-menu
            // enable/disable toggle and the Wi-Fi connection list are Q6, needing NM writes).
            DetailOwner::Network => {
                let _ = network;
                vec![spawn_row(
                    "Network Settings",
                    &["gnome-control-center", "network"],
                    false,
                )]
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
                    .map(|profile| {
                        ItemRow {
                            label: profile.name().to_string(),
                            icons: Vec::new(),
                            action: PopoverAction::SetPowerProfile(profile.id().to_string()),
                            separator_before: false,
                            selected: power.active == profile.id(),
                            trailing: None,
                            placeholder: false,
                        }
                        .into()
                    })
                    .collect();
                rows.push(spawn_row(
                    "Power Settings",
                    &["gnome-control-center", "power"],
                    true,
                ));
                rows
            }
            // gnome-shell's output device list: one row per **card port** (gvc UIDevice), labelled
            // `description – origin` with the device's icon, the current one carrying a trailing
            // check; then a separator + a "Sound Settings" entry point
            // (`volume.js:80-82,126-165`). Port-level, not sink-level: one card shows "Speakers"
            // and "Headphones" as separate rows.
            DetailOwner::Output => {
                let mut rows: Vec<DetailRow> =
                    crate::audio::output_devices(audio.sinks, audio.cards)
                        .into_iter()
                        .take(MAX_DEVICE_ROWS)
                        .map(|device| {
                            ItemRow {
                                label: device.label(),
                                icons: device.icon.clone().into_iter().collect(),
                                selected: device.selected,
                                action: PopoverAction::SetOutputDevice(device.key),
                                separator_before: false,
                                trailing: None,
                                placeholder: false,
                            }
                            .into()
                        })
                        .collect();
                rows.push(spawn_row(
                    "Sound Settings",
                    &["gnome-control-center", "sound"],
                    true,
                ));
                rows
            }
            // A (name label, slider) pair per monitor, and nothing else — gnome-shell's
            // `BrightnessSliderMenu.addSlider` adds a non-reactive `PopupMenuItem(scale.name)`
            // then a `PopupBaseMenuItem` holding the bare slider (`brightness.js:13-34`).
            DetailOwner::Brightness => monitors
                .iter()
                .flat_map(|m| {
                    [
                        DetailRow::Label(m.name.clone()),
                        DetailRow::Slider {
                            connector: m.connector.clone(),
                            value: m.value,
                        },
                    ]
                })
                .collect(),
            // The input mirror of Output: one row per input port, then "Sound Settings".
            DetailOwner::Input => {
                let mut rows: Vec<DetailRow> =
                    crate::audio::input_devices(audio.sources, audio.cards)
                        .into_iter()
                        .take(MAX_DEVICE_ROWS)
                        .map(|device| {
                            ItemRow {
                                label: device.label(),
                                icons: device.icon.clone().into_iter().collect(),
                                selected: device.selected,
                                action: PopoverAction::SetInputDevice(device.key),
                                separator_before: false,
                                trailing: None,
                                placeholder: false,
                            }
                            .into()
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
    fn row_shape(self, device_count: usize) -> Vec<RowSpec> {
        match self {
            DetailOwner::Network => vec![RowSpec::item(false)],
            DetailOwner::Power => vec![
                RowSpec::item(false),
                RowSpec::item(false),
                RowSpec::item(false),
                RowSpec::item(true),
            ],
            // N device/profile rows, then a trailing settings row past a separator.
            DetailOwner::Output | DetailOwner::Input | DetailOwner::PowerProfile => {
                let mut shape = vec![RowSpec::item(false); device_count.min(MAX_DEVICE_ROWS)];
                shape.push(RowSpec::item(true));
                shape
            }
            // N visible-device rows — or ONE placeholder row when there are none — then the
            // settings row past a separator.
            DetailOwner::Bluetooth => {
                let mut shape = vec![RowSpec::item(false); device_count.clamp(1, MAX_DEVICE_ROWS)];
                shape.push(RowSpec::item(true));
                shape
            }
            // A (name label, slider) pair per monitor and nothing else: gnome-shell adds only the
            // header and the slider section, with no separator and no settings row
            // (`brightness.js:47-49`, `BrightnessSliderMenu.addSlider` `:13-34`).
            DetailOwner::Brightness => (0..device_count)
                .flat_map(|_| {
                    [
                        RowSpec {
                            kind: RowKind::Label,
                            separator_before: false,
                        },
                        RowSpec {
                            kind: RowKind::Slider,
                            separator_before: false,
                        },
                    ]
                })
                .collect(),
        }
    }

    /// The card's logical height: top pad + header + gap + rows + separators + bottom pad.
    fn detail_height(self, device_count: usize) -> f64 {
        let shape = self.row_shape(device_count);
        let rows = shape.len() as f64;
        let seps = shape.iter().filter(|s| s.separator_before).count() as f64;
        let row_h: f64 = shape.iter().map(|s| s.kind.height()).sum();
        DETAIL_PAD
            + DETAIL_HEADER_H
            + DETAIL_HEADER_GAP
            + row_h
            + (rows - 1.).max(0.) * DETAIL_ROW_GAP
            + seps * DETAIL_SEP_EXTRA
            + DETAIL_PAD
    }

    /// The natural (pre-shift) y of the bottom edge of the owner's row — where the detail card is
    /// pinned directly below (gnome-shell binds the menu container's Y to the source actor).
    fn anchor_row_bottom(self, layout: Layout) -> f64 {
        let sliders = layout.sliders;
        let grid_row_bottom = |index: usize| {
            let row = (index / COLS) as f64;
            grid_top(sliders) + (row + 1.) * TILE_H + row * TILE_GAP
        };
        match self {
            DetailOwner::Network => grid_row_bottom(network_index()),
            // The Bluetooth tile is inserted right after Network (index 1) — same first row.
            DetailOwner::Bluetooth => grid_row_bottom(bluetooth_index()),
            // The Power Mode tile is the first appended conditional; its index shifts by one when
            // the Bluetooth tile is inserted ahead of it.
            DetailOwner::PowerProfile => {
                grid_row_bottom(power_profile_index(layout.show_bluetooth))
            }
            // The power button lives in the top system row, so its detail pins right below it —
            // above the sliders and the whole grid, which shift down.
            DetailOwner::Power => PAD + SYS_H,
            // The picker pins below its slider's row (each picker is open only while its slider
            // exists — `normalize_expanded` guarantees it). Derived from the slider's slot so the
            // Input anchor is right whether or not the output slider is present.
            DetailOwner::Output => slider_row_y(Slider::Output, sliders) + SLIDER_H,
            DetailOwner::Input => slider_row_y(Slider::Mic, sliders) + SLIDER_H,
            DetailOwner::Brightness => slider_row_y(Slider::Brightness, sliders) + SLIDER_H,
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

/// The grid slot the Bluetooth tile occupies when shown: **inserted at 1**, right after Network —
/// GNOME's QS tile order is network, bluetooth, powerProfiles (`panel.js:380-383`), and of the
/// tiles we carry, Bluetooth is the only one that goes *between* existing tiles rather than
/// appending. Pinned by a debug_assert at the hit site.
fn bluetooth_index() -> usize {
    1
}

/// The grid slot the Power Mode tile occupies when shown. It's the **first** conditional tile
/// appended after [`BASE_GRID`] (before Airplane — see [`grid`]), shifted one right when the
/// Bluetooth tile is inserted ahead of it; its detail view anchors below this row. The order is
/// load-bearing here and pinned by a debug_assert at the hit/render sites.
fn power_profile_index(show_bluetooth: bool) -> usize {
    BASE_GRID.len() + show_bluetooth as usize
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
    /// Visible bluetooth-device count — the Bluetooth detail card's row count (min 1: the
    /// placeholder takes a row when there are none).
    bt_device_count: usize,
    /// The number of backlit monitors — the brightness card's row-pair count and the gate on its
    /// arrow.
    monitor_count: usize,
    /// Whether the Bluetooth tile is in the grid (rfkill `available`) — it's *inserted* at index
    /// 1, so every later tile's index shifts by it.
    show_bluetooth: bool,
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
                Slider::Brightness => self.monitor_count,
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
            DetailOwner::Bluetooth => self.bt_device_count,
            DetailOwner::Brightness => self.monitor_count,
            _ => 0,
        }
    }

    /// The detail card's `(natural insert y, block height)` when a view is open. The block height
    /// is the card plus its top margin — exactly how far the rows below the owner shift down, and
    /// how much taller the menu grows.
    fn detail_block(self) -> Option<(f64, f64)> {
        let owner = self.expanded?;
        Some((
            owner.anchor_row_bottom(self),
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
    /// Bluetooth (BlueZ + gsd-rfkill) — a live D-Bus-backed `QuickMenuToggle`, shown only when a
    /// soft Bluetooth kill switch exists (gnome-shell's `available`, `bluetooth.js:103-108`).
    /// Its body toggles the adapter (an rfkill + `Powered` write pair); its arrow opens the
    /// device list.
    Bluetooth,
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

/// The live grid: [`BASE_GRID`] with the Bluetooth tile **inserted at index 1** when shown
/// (GNOME puts bluetooth right after network, `panel.js:380-383`) plus the two appended
/// conditionals, **always in this exact order — PowerProfile then Airplane**. `network_index`
/// (0, unaffected by the insertion), `bluetooth_index` (1), and
/// `power_profile_index`/`anchor_row_bottom` (shifted by `show_bluetooth`) resolve tile identity
/// positionally; debug_asserts at the hit site (`pointer_click`) pin the order.
fn grid(show_bluetooth: bool, show_power_profile: bool, show_airplane: bool) -> Vec<GridTile> {
    let mut tiles = BASE_GRID.to_vec();
    if show_bluetooth {
        tiles.insert(bluetooth_index(), GridTile::Bluetooth);
    }
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
            GridTile::Bluetooth => "Bluetooth".to_string(),
        }
    }

    /// The tile's second (subtitle) line, or `None` for a single-line tile: Power Mode's active
    /// profile, and Bluetooth's connected-device summary (`bluetooth.js:410-419`), mirroring
    /// gnome-shell's `QuickMenuToggle` subtitle.
    fn subtitle(self, power: &PowerProfileStatus, bluetooth: &BluetoothStatus) -> Option<String> {
        match self {
            GridTile::PowerProfile => Some(power.name().to_string()),
            GridTile::Bluetooth => bluetooth.subtitle(),
            _ => None,
        }
    }

    /// Candidate symbolic icon names, first that resolves wins. `bt_state` is the Bluetooth
    /// adapter state *as displayed* — the predicted override during a toggle, else the snapshot
    /// (`bluetooth.js:114-118`).
    fn icons(
        self,
        network: NetworkStatus,
        power: &PowerProfileStatus,
        bt_state: BtAdapterState,
    ) -> Vec<String> {
        match self {
            GridTile::Toggle(t) => t.icons().iter().map(|s| s.to_string()).collect(),
            GridTile::Network => network_icons(network),
            GridTile::PowerProfile => vec![power.icon().to_string()],
            GridTile::Airplane => vec!["airplane-mode-symbolic".to_string()],
            GridTile::Bluetooth => vec![BluetoothStatus::icon_for(bt_state).to_string()],
        }
    }

    /// Whether the tile reads as "on" (accent background): a toggle's gsettings state, Network's
    /// connected state, Power Mode's non-Balanced state, Airplane's active state, or Bluetooth's
    /// powered adapter (`checked = active`, `bluetooth.js:311-313`).
    fn is_on(
        self,
        toggles: QuickToggles,
        network: NetworkStatus,
        airplane: AirplaneStatus,
        power: &PowerProfileStatus,
        bt_powered: bool,
    ) -> bool {
        match self {
            GridTile::Toggle(t) => t.is_on(toggles),
            GridTile::Network => {
                matches!(network, NetworkStatus::Wired | NetworkStatus::Wireless(_))
            }
            GridTile::PowerProfile => power.is_active(),
            GridTile::Airplane => airplane.active,
            GridTile::Bluetooth => bt_powered,
        }
    }

    /// Whether this tile carries an expand-arrow that opens a detail view (gnome-shell's
    /// `QuickMenuToggle`): Network, Power Mode, and Bluetooth. The toggles/Airplane are plain
    /// [`QuickToggle`]s. (Power Mode's arrow is additionally gated on >2 profiles in
    /// [`tile_arrow_rect`], gnome-shell's `menuEnabled`; Bluetooth's menu is unconditional like
    /// gnome-shell's.)
    fn detail_owner(self) -> Option<DetailOwner> {
        match self {
            GridTile::Network => Some(DetailOwner::Network),
            GridTile::PowerProfile => Some(DetailOwner::PowerProfile),
            GridTile::Bluetooth => Some(DetailOwner::Bluetooth),
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
    /// Bluetooth adapter + devices (from the BlueZ watcher), for the conditionally-shown
    /// Bluetooth tile and its device-list detail view.
    bluetooth: BluetoothStatus,
    /// The Bluetooth kill-switch state from gsd-rfkill: `available()` gates the tile
    /// (`bluetooth.js:103-108,308-310`).
    bluetooth_rfkill: BluetoothRfkill,
    /// The adapter state shown while a body-click's rfkill/Powered writes round-trip — the
    /// "acquiring" icon is the only feedback the click landed (gnome-shell's `_predictedState`,
    /// `bluetooth.js:120-136`). Cleared when a snapshot's real adapter state *changes*, or by the
    /// caller's 30 s failsafe.
    bt_predicted: Option<BtAdapterState>,
    /// The device path with a `Connect`/`Disconnect` in flight — its row swaps the trailing
    /// label for a busy mark (gnome-shell's row spinner). Cleared on
    /// [`bluetooth_connect_done`](Self::bluetooth_connect_done) or the device vanishing.
    bt_busy: Option<String>,
    /// The device-row order frozen when the Bluetooth detail opened ("we don't reorder the list
    /// while the menu is open", `bluetooth.js:326-331`); newcomers append.
    bt_row_order: Vec<String>,
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
    /// The sound cards and their ports, which the device pickers are built from — the port-level
    /// list is a function of these plus the sink/source lists.
    cards: crate::audio::AudioCards,
    /// Whether the default sink is headphones, which swaps **this slider's icon only** (never the
    /// panel indicator, never the OSD — see [`crate::audio::output_slider_icon`]). Resolved by the
    /// compositor from the card/route model, since that lives outside this widget.
    headphones: bool,
    /// Microphone state (level/mute + recording/source-present visibility) for the mic slider.
    mic: MicStatus,
    /// The input sources + current default, for the mic slider's device picker.
    source_list: SourceList,
    /// The brightness scales (from the compositor-owned `BrightnessManager`): the global scale
    /// backs the slider and its presence decides whether the slider exists at all.
    brightness: crate::brightness::BrightnessView,
    /// The slider currently being dragged (a button held on its track) and the device count frozen
    /// at drag start — so a device hot-plug mid-drag can't add/remove that slider's picker arrow,
    /// resize the track, and remap `volume_from_x` (snapping the level under a stationary
    /// pointer). `None` when not dragging; [`layout`](Self::layout) threads it into
    /// `Layout::drag`.
    sliding: Option<(SliderId, usize)>,
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
        bluetooth: BluetoothStatus,
        bluetooth_rfkill: BluetoothRfkill,
        battery: Option<BatteryStatus>,
        audio: Option<AudioStatus>,
        sink_list: SinkList,
        cards: crate::audio::AudioCards,
        headphones: bool,
        mic: MicStatus,
        source_list: SourceList,
        brightness: crate::brightness::BrightnessView,
        accent: [u8; 3],
    ) -> Self {
        Self {
            toggles,
            network,
            airplane,
            power,
            bluetooth,
            bluetooth_rfkill,
            bt_predicted: None,
            bt_busy: None,
            bt_row_order: Vec::new(),
            battery,
            audio,
            sink_list,
            cards,
            headphones,
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
            brightness,
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
            brightness: self.brightness.global.is_some(),
        }
    }

    /// The live grid tiles (Network + toggles, plus Power Mode / Airplane when their daemons are
    /// present).
    fn grid(&self) -> Vec<GridTile> {
        grid(
            self.bluetooth_rfkill.available(),
            self.power.show,
            self.airplane.show,
        )
    }

    /// The Bluetooth adapter state as displayed: the predicted override during a toggle, else the
    /// live snapshot (`bluetooth.js:114-118`).
    fn bt_effective_state(&self) -> BtAdapterState {
        self.bt_predicted.unwrap_or(self.bluetooth.state)
    }

    /// The open detail view's rows for `owner`, with every live source threaded in.
    fn detail_rows(&self, owner: DetailOwner) -> Vec<DetailRow> {
        owner.rows(
            self.network,
            AudioDetail {
                sinks: &self.sink_list,
                sources: &self.source_list,
                cards: &self.cards,
            },
            &self.power,
            BtDetail {
                status: &self.bluetooth,
                order: &self.bt_row_order,
                busy: self.bt_busy.as_deref(),
            },
            &self.brightness.monitors,
        )
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
            bt_device_count: self.bluetooth.visible_devices().len(),
            monitor_count: self.monitor_count(),
            show_bluetooth: self.bluetooth_rfkill.available(),
            // Only a top-level drag can affect the geometry (it's the arrow it pins); a card
            // slider's row is placed by the card, which sizes from the live counts anyway.
            drag: match &self.sliding {
                Some((SliderId::Top(sl), frozen)) => Some((*sl, *frozen)),
                _ => None,
            },
            grid_len: self.grid().len(),
        }
    }

    /// An external content change (device list, profile, airplane, audio) can move or remove the
    /// hovered control, so bump the chrome revision AND drop the stale hover — it re-resolves on
    /// the next pointer motion. Used by the update setters below, NOT by click/hover
    /// interactions (a click leaves the pointer over the same control, so its hover stays
    /// valid).
    fn content_bumped(&mut self) {
        self.revision += 1;
        self.hovered = None;
    }

    /// Adopt a fresh airplane-mode snapshot (from the gsd-rfkill watcher). `show` grows/shrinks the
    /// grid (a 5th tile); `active` flips the tile. Returns whether it changed.
    pub fn set_airplane(&mut self, airplane: AirplaneStatus) -> bool {
        if self.airplane == airplane {
            return false;
        }
        self.airplane = airplane;
        self.content_bumped();
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
        self.content_bumped();
        self.normalize_expanded();
        true
    }

    /// Adopt a fresh Bluetooth adapter/device snapshot (from the BlueZ watcher). Returns whether
    /// it changed. Clears the predicted state when the *real* adapter state changes (gnome-shell's
    /// `notify::default-adapter-state` → `delete this._predictedState`, `bluetooth.js:53-56`) and
    /// the busy mark if its device vanished.
    pub fn set_bluetooth(&mut self, bluetooth: BluetoothStatus) -> bool {
        if self.bluetooth == bluetooth {
            return false;
        }
        if bluetooth.state != self.bluetooth.state {
            self.bt_predicted = None;
        }
        // An active (powered) flip rebuilds GNOME's whole device list (`notify::active` destroys
        // every item and re-syncs, `bluetooth.js:339-346`), so the frozen order resets to the
        // fresh sort rather than resurrecting the pre-flip order.
        if bluetooth.powered != self.bluetooth.powered
            && self.expanded == Some(DetailOwner::Bluetooth)
        {
            self.bt_row_order = bluetooth
                .visible_devices()
                .iter()
                .map(|d| d.path.clone())
                .collect();
        }
        if let Some(busy) = &self.bt_busy {
            if !bluetooth.devices.iter().any(|d| &d.path == busy) {
                self.bt_busy = None;
            }
        }
        self.bluetooth = bluetooth;
        self.content_bumped();
        self.normalize_expanded();
        true
    }

    /// Adopt a fresh Bluetooth rfkill snapshot (from the gsd-rfkill watcher). `available()`
    /// grows/shrinks the grid by the Bluetooth tile (inserted at slot 1). Returns whether it
    /// changed. Calls `normalize_expanded`: an open device list must collapse if the tile
    /// vanishes.
    pub fn set_bluetooth_rfkill(&mut self, rfkill: BluetoothRfkill) -> bool {
        if self.bluetooth_rfkill == rfkill {
            return false;
        }
        self.bluetooth_rfkill = rfkill;
        self.content_bumped();
        self.normalize_expanded();
        true
    }

    /// A `Device1.Connect`/`Disconnect` we issued finished (either way): clear the row's busy
    /// mark (gnome-shell's `spinner.stop()` after the await, `bluetooth.js:257-261`). Returns
    /// whether anything changed (→ redraw).
    pub fn bluetooth_connect_done(&mut self, path: &str) -> bool {
        if self.bt_busy.as_deref() != Some(path) {
            return false;
        }
        self.bt_busy = None;
        self.revision += 1;
        true
    }

    /// The 30 s failsafe on the predicted adapter state (`bluetooth.js:27,131-136`): if no real
    /// state change ever echoes back (the write failed silently), stop showing "acquiring".
    /// Returns whether anything changed (→ redraw).
    pub fn clear_bluetooth_prediction(&mut self) -> bool {
        if self.bt_predicted.is_none() {
            return false;
        }
        self.bt_predicted = None;
        self.revision += 1;
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
            // The device list is valid only while its tile exists (rfkill `available`).
            Some(DetailOwner::Bluetooth) => !self.bluetooth_rfkill.available(),
            // The brightness card the same, off its slider + the >1-scale arrow gate
            // (`brightness.js:61`).
            Some(DetailOwner::Brightness) => !sliders.brightness || self.monitor_count() <= 1,
            _ => false,
        };
        if invalid {
            self.expanded = None;
            self.content_bumped();
            return true;
        }
        false
    }

    /// Adopt a fresh output-sink list (from the PipeWire watcher). Returns whether it changed.
    pub fn set_sink_list(&mut self, sink_list: SinkList) -> bool {
        let mut changed = self.sink_list != sink_list;
        if changed {
            self.sink_list = sink_list;
            self.content_bumped();
        }
        changed |= self.normalize_expanded();
        changed
    }

    /// Adopt a fresh card/route model (from the PipeWire watcher). Returns whether it changed.
    pub fn set_audio_cards(&mut self, cards: crate::audio::AudioCards) -> bool {
        let mut changed = self.cards != cards;
        if changed {
            self.cards = cards;
            self.content_bumped();
        }
        changed |= self.normalize_expanded();
        changed
    }

    /// Adopt the headphone state resolved from the card/route model. Returns whether it changed.
    pub fn set_headphones(&mut self, headphones: bool) -> bool {
        let changed = self.headphones != headphones;
        self.headphones = headphones;
        changed
    }

    /// The menu's logical size: two tile columns + the system row, grown by the open detail view.
    pub fn logical_size(&self) -> Size<f64, Logical> {
        Size::from((menu_w(), menu_h(self.layout())))
    }

    /// The menu box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        MENU_RADIUS
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
            for (k, row) in self.detail_rows(owner).into_iter().enumerate() {
                let Some(rrect) = detail_row_rect(k, layout) else {
                    continue;
                };
                if !rrect.contains(pos) {
                    continue;
                }
                match row {
                    DetailRow::Item(row) => {
                        // A device-row click marks that row busy until the Connect/Disconnect
                        // finishes (gnome-shell plays the spinner around the await,
                        // `bluetooth.js:257-261`).
                        if let PopoverAction::ConnectBluetoothDevice { path, .. } = &row.action {
                            self.bt_busy = Some(path.clone());
                            self.revision += 1;
                        }
                        return row.action;
                    }
                    // A card slider: jump to (and begin dragging toward) the clicked position,
                    // like the top-level tracks. Only the track band is reactive — in gnome-shell
                    // the menu item is `reactive: false` and only the `slider-bin` inside it takes
                    // the click (`brightness.js:23,29`), so a click on the item's padding must do
                    // nothing. Without this the clamp in `volume_from_track_x` would turn a
                    // near-miss into a slam to minimum (i.e. a dark panel).
                    DetailRow::Slider { connector, .. } => {
                        let track = detail_slider_track_rect(rrect);
                        if !track.contains(pos) {
                            return PopoverAction::Consumed;
                        }
                        self.sliding =
                            Some((SliderId::Monitor(connector.clone()), self.monitor_count()));
                        let value = volume_from_track_x(track, pos.x);
                        return self.set_local_monitor(&connector, value);
                    }
                    // A non-reactive name label swallows its click.
                    DetailRow::Label(_) => return PopoverAction::Consumed,
                }
            }
            if detail_rect(layout).is_some_and(|r| r.contains(pos)) {
                return PopoverAction::Consumed;
            }
        }
        let tiles = self.grid();
        debug_assert!(
            matches!(tiles[0], GridTile::Network),
            "Network leads the grid (network_index depends on it)"
        );
        debug_assert!(
            tiles
                .iter()
                .position(|t| matches!(t, GridTile::Bluetooth))
                .is_none_or(|i| i == bluetooth_index()),
            "Bluetooth is the only INSERTED tile, always at slot 1 (right after Network)"
        );
        debug_assert!(
            tiles
                .iter()
                .position(|t| matches!(t, GridTile::PowerProfile))
                .is_none_or(|i| i == power_profile_index(layout.show_bluetooth)),
            "PowerProfile must be the FIRST appended tile (before Airplane) — \
             power_profile_index(false)/anchor_row_bottom assume its position"
        );
        for (i, &item) in tiles.iter().enumerate() {
            // A menu tile's arrow-half toggles its detail view (open, or close if already open —
            // one at a time); the toggle-body keeps the tile's own behavior. A plain tile is all
            // body.
            if tile_arrow_rect(i, item, layout).is_some_and(|r| r.contains(pos)) {
                let owner = item.detail_owner();
                self.expanded = if self.expanded == owner { None } else { owner };
                // Freeze the device-row order at open ("we don't reorder the list while the menu
                // is open, so do it now", `bluetooth.js:326-331`).
                if self.expanded == Some(DetailOwner::Bluetooth) {
                    self.bt_row_order = self
                        .bluetooth
                        .visible_devices()
                        .iter()
                        .map(|d| d.path.clone())
                        .collect();
                }
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
                    // Bluetooth body: gnome-shell's `toggleActive` (`bluetooth.js:120-141`) — an
                    // rfkill write plus (when off) powering the adapter, resolved in
                    // `apply_popover_action`. Echo-driven for the real state, but the *icon*
                    // optimistically shows the acquiring transition (the rfkill→adapter delay is
                    // user-visible; `_predictedState`, `bluetooth.js:126-129`).
                    GridTile::Bluetooth => {
                        self.bt_predicted = Some(if self.bluetooth.powered {
                            BtAdapterState::TurningOff
                        } else {
                            BtAdapterState::TurningOn
                        });
                        self.revision += 1;
                        PopoverAction::ToggleBluetooth
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
        // The volume sliders (output, then mic): the icon toggles mute; the arrow (when >1 device)
        // toggles the device picker; the track jumps to (and begins dragging toward) the clicked
        // position. The arrow is tested BEFORE the track so an arrow click never starts a drag (the
        // track is genuinely shortened to make room, so `volume_from_x` stays in range).
        for slider in SLIDERS {
            if !layout.sliders.present(slider) {
                continue;
            }
            if slider_icon_rect(slider, layout).contains(pos) {
                return match slider {
                    Slider::Output => PopoverAction::ToggleMute,
                    Slider::Mic => PopoverAction::ToggleInputMute,
                    // Non-reactive icon: the click lands on the row, and stops there.
                    Slider::Brightness => PopoverAction::Consumed,
                };
            }
            if slider_arrow_rect(slider, layout).is_some_and(|r| r.contains(pos)) {
                if let Some(owner) = slider.owner() {
                    self.expanded = if self.expanded == Some(owner) {
                        None
                    } else {
                        Some(owner)
                    };
                    self.revision += 1;
                }
                return PopoverAction::Consumed;
            }
            if slider_track_rect(slider, layout).contains(pos) {
                self.sliding = Some((SliderId::Top(slider), self.device_count(slider)));
                return self.set_local_volume(slider, volume_from_x(slider, pos.x, layout));
            }
        }
        PopoverAction::Consumed
    }

    /// The number of backlit monitors — the brightness card's row-pair count and its arrow gate
    /// (`menuEnabled = this._manager.scales.length > 1`, `brightness.js:61`; `scales` is the
    /// per-monitor scales, the global one excluded, `brightnessManager.js:104-106`).
    fn monitor_count(&self) -> usize {
        self.brightness.monitors.len()
    }

    /// Optimistically move one monitor's scale (so its handle tracks the pointer ahead of the
    /// hardware echo) and emit the matching action.
    fn set_local_monitor(&mut self, connector: &str, value: f64) -> PopoverAction {
        self.revision += 1;
        if let Some(m) = self
            .brightness
            .monitors
            .iter_mut()
            .find(|m| m.connector == connector)
        {
            m.value = value;
        }
        PopoverAction::SetMonitorBrightness(connector.to_owned(), value)
    }

    /// The live device count backing a slider's picker (sinks for Output, sources for Mic) — the
    /// value frozen at drag start.
    fn device_count(&self, slider: Slider) -> usize {
        match slider {
            Slider::Output => self.sink_list.sinks.len(),
            Slider::Mic => self.source_list.sources.len(),
            Slider::Brightness => self.brightness.monitors.len(),
        }
    }

    /// Continue a slider drag: while a button is held on the track, motion updates
    /// the volume. Returns the action to apply, or `None` when not dragging.
    pub fn pointer_drag(&mut self, pos: Point<f64, Logical>) -> Option<PopoverAction> {
        let layout = self.layout();
        match self.sliding.clone()?.0 {
            SliderId::Top(slider) => {
                Some(self.set_local_volume(slider, volume_from_x(slider, pos.x, layout)))
            }
            // A card row's index is looked up live, never frozen: a monitor coming or going
            // reorders the rows under the pointer, and the drag must follow its own connector.
            SliderId::Monitor(connector) => {
                let owner = layout.expanded?;
                let k = self.detail_rows(owner).iter().position(
                    |row| matches!(row, DetailRow::Slider { connector: c, .. } if *c == connector),
                )?;
                let track = detail_slider_track_rect(detail_row_rect(k, layout)?);
                let value = volume_from_track_x(track, pos.x);
                Some(self.set_local_monitor(&connector, value))
            }
        }
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
            let rows = self.detail_rows(owner);
            for (k, row) in rows.iter().enumerate() {
                // Only ordinary rows highlight: a placeholder line is non-reactive
                // (`bluetooth.js:286-290`), and so are the brightness card's label and slider
                // rows (`brightness.js:14,29` build both with `reactive: false`).
                let reactive = row.item().is_some_and(|r| !r.placeholder);
                if reactive && detail_row_rect(k, layout).is_some_and(|r| r.contains(pos)) {
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
        for slider in SLIDERS {
            if slider.icon_is_button()
                && layout.sliders.present(slider)
                && slider_icon_rect(slider, layout).contains(pos)
            {
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
        match slider {
            SliderId::Top(slider) => frozen != self.device_count(slider),
            // A card slider has no arrow, so nothing about it can have been suppressed.
            SliderId::Monitor(_) => false,
        }
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
            Slider::Brightness => {
                // Optimistic like the volume sliders: the handle follows the pointer, and the
                // hardware echo (a udev change) confirms it a moment later.
                if let Some(global) = self.brightness.global.as_mut() {
                    *global = volume;
                }
                PopoverAction::SetBrightness(volume)
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
        if matches!(self.sliding, Some((SliderId::Top(Slider::Output), _))) {
            if audio.is_some() {
                // Still present: keep the optimistic drag value, don't yank.
                return false;
            }
            // The sink vanished under the drag: cancel it before the slider hides.
            self.sliding = None;
        }
        self.audio = audio;
        self.content_bumped();
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
        if matches!(self.sliding, Some((SliderId::Top(Slider::Mic), _))) {
            if mic.recording && mic.source_present {
                // Still visible: keep the optimistic level/mute, don't move the slider mid-drag.
                return false;
            }
            // The mic slider is vanishing under the drag: cancel it before it hides.
            self.sliding = None;
        }
        self.mic = mic;
        self.content_bumped();
        // The mic slider vanishing must also close an open input picker.
        self.normalize_expanded();
        true
    }

    /// See [`crate::ui::popover::PanelPopover::open_brightness_card_for_test`].
    #[cfg(test)]
    pub fn open_brightness_card_for_test(&mut self) {
        self.expanded = Some(DetailOwner::Brightness);
        self.revision += 1;
    }

    /// Adopt a fresh brightness snapshot (from the compositor's `BrightnessManager`). Returns
    /// whether it changed.
    ///
    /// Same drag guard as [`set_mic`](Self::set_mic): while the brightness slider is being
    /// dragged, the hardware echo lags the pointer, so the optimistic value wins — unless the
    /// backlight is *going away* (the panel was unplugged), which cancels the drag before the
    /// slider hides.
    pub fn set_brightness(&mut self, mut brightness: crate::brightness::BrightnessView) -> bool {
        if brightness == self.brightness {
            return false;
        }
        match &self.sliding {
            // Hold ONLY the dragged value at the pointer; everything else in the snapshot is
            // adopted. Rejecting the whole snapshot (the way the mic slider does) would drop
            // structural changes for good — `set_brightness` is only called on brightness events,
            // so a monitor that unplugged mid-drag would leave a phantom row and a live arrow
            // behind until some unrelated event happened to re-push.
            Some((SliderId::Top(Slider::Brightness), _)) => match brightness.global.as_mut() {
                Some(global) => {
                    if let Some(value) = self.brightness.global {
                        *global = value;
                    }
                }
                // The backlight vanished under the drag: cancel it before the slider hides.
                None => self.sliding = None,
            },
            // A card slider is dragged *per monitor*, and moving one monitor moves the global
            // scale with it (`brightnessManager.js:203-228` normalizes the global to the max), so
            // the snapshot IS adopted mid-drag — with the dragged row's own value held at the
            // pointer. Suppressing the whole snapshot the way the mic slider does would freeze the
            // top-level slider while a card row is dragged.
            Some((SliderId::Monitor(connector), _)) => {
                let dragged = self
                    .brightness
                    .monitors
                    .iter()
                    .find(|m| m.connector == *connector)
                    .map(|m| m.value);
                match brightness
                    .monitors
                    .iter_mut()
                    .find(|m| m.connector == *connector)
                {
                    Some(m) => {
                        if let Some(value) = dragged {
                            m.value = value;
                        }
                    }
                    // The dragged monitor is gone: cancel the drag before its row disappears.
                    None => self.sliding = None,
                }
            }
            _ => (),
        }
        self.brightness = brightness;
        self.content_bumped();
        // Losing the slider, or dropping to a single scale, must close the card — it would
        // otherwise pin below a row that is no longer there.
        self.normalize_expanded();
        // A card drag cannot outlive its card: the row it tracks is gone, so further motion would
        // keep pinning a stale value over incoming snapshots with nothing on screen to explain it.
        if matches!(self.sliding, Some((SliderId::Monitor(_), _)))
            && self.expanded != Some(DetailOwner::Brightness)
        {
            self.sliding = None;
        }
        true
    }

    /// Adopt a fresh input-source list (from the PipeWire watcher). Returns whether it changed.
    pub fn set_source_list(&mut self, source_list: SourceList) -> bool {
        let mut changed = self.source_list != source_list;
        if changed {
            self.source_list = source_list;
            self.content_bumped();
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
            let on = item.is_on(
                self.toggles,
                self.network,
                self.airplane,
                &self.power,
                self.bluetooth.powered,
            );
            let color = if on { FG_ON } else { FG_OFF };
            let rect = tile_rect(i, layout);
            let center = Point::from((
                rect.loc.x + TILE_ICON_INSET + TILE_ICON / 2.,
                rect.loc.y + rect.size.h / 2.,
            ));
            let candidates = item.icons(self.network, &self.power, self.bt_effective_state());
            if let Some(el) = widget::icon_element(
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
                if let Some(el) = widget::icon_element(
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
                // Centered on the icon pill (the pill itself is chrome, drawn in `draw`).
                let center = Point::from((
                    card.loc.x + DETAIL_HEADER_INSET + DETAIL_HEADER_PILL / 2.,
                    card.loc.y + DETAIL_PAD + DETAIL_HEADER_H / 2.,
                ));
                if let Some(el) = widget::icon_element(
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
                for (k, row) in self.detail_rows(owner).into_iter().enumerate() {
                    let Some(rrect) = detail_row_rect(k, layout) else {
                        continue;
                    };
                    // Only ordinary rows carry an icon or a check.
                    let DetailRow::Item(row) = row else {
                        continue;
                    };
                    // A leading row icon (none for the current consumers), if any.
                    if !row.icons.is_empty() {
                        let center = Point::from((
                            rrect.loc.x + DETAIL_ROW_INSET + TILE_ICON / 2.,
                            rrect.loc.y + rrect.size.h / 2.,
                        ));
                        if let Some(el) = widget::icon_element(
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
                        if let Some(el) = widget::icon_element(
                            renderer,
                            icons,
                            style::CHECK_ICONS,
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
            if let Some(el) = widget::icon_element(
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
            if let Some(el) = widget::icon_element(
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
        for slider in SLIDERS {
            if !layout.sliders.present(slider) {
                continue;
            }
            let disc = slider_icon_rect(slider, layout);
            let center =
                Point::from((disc.loc.x + disc.size.w / 2., disc.loc.y + disc.size.h / 2.));
            let name = match slider {
                Slider::Output => crate::audio::output_slider_icon(
                    &self.audio.unwrap_or_default(),
                    self.headphones,
                ),
                Slider::Mic => crate::audio::mic_volume_icon(&self.mic),
                // A single icon, not sensitivity-graded like the volume ones (`brightness.js:41`).
                Slider::Brightness => "display-brightness-symbolic",
            }
            .to_string();
            if let Some(el) = widget::icon_element(
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
                if let Some(el) = widget::icon_element(
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

        // The chrome (tile backgrounds + labels) on a transparent bg, beneath the icons. Reports
        // NO opaque region: the `.popup-menu-content` box fill (and its rounded opaque region) is
        // now drawn by the shared popover chrome behind this texture (`PanelPopover::render`).
        match self.texture(renderer, scale) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
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
        // Shape every run up front (needs `&mut renderer`, before the bake frame opens).
        // `TextShaper` owns the pt → physical-px multiply — no `* scale` on the font sizes.
        // `.quick-toggle-title` / the power toggle's percentage are %heading = weight 700; the
        // subtitle line is %caption, regular weight.
        let label_style = TextStyle::new(LABEL_PT).bold();
        let subtitle_style = TextStyle::new(SUBTITLE_PT);
        let labels: Vec<String> = self
            .grid()
            .iter()
            .map(|item| item.label(self.network))
            .collect();
        let (label_runs, subtitle_runs, pill_run, detail_runs) = {
            let mut shaper = TextShaper::new(renderer, scale);
            let label_runs: Vec<ShapedText> = labels
                .iter()
                .map(|l| shaper.shape(l, label_style))
                .collect::<Result<_, _>>()?;
            // A parallel Option<run> for the subtitle line — `Some` only for the tiles that carry
            // one (Power Mode's active profile), `None` otherwise.
            let subtitle_runs: Vec<Option<ShapedText>> = self
                .grid()
                .iter()
                .map(|item| {
                    item.subtitle(&self.power, &self.bluetooth)
                        .map(|s| shaper.shape(&s, subtitle_style))
                        .transpose()
                })
                .collect::<Result<_, _>>()?;
            let pill_run = self
                .battery
                .as_ref()
                .map(|b| shaper.shape(&format!("{}%", b.percentage.round() as i64), label_style))
                .transpose()?;
            // The open detail view's header title (bold, `%title_3`) + its regular-weight rows.
            let detail_runs = self
                .expanded
                .map(|owner| -> anyhow::Result<_> {
                    let (_, title) = owner.header(self.network);
                    let title_run = shaper.shape(&title, TextStyle::new(DETAIL_TITLE_PT).bold())?;
                    let rows = self.detail_rows(owner);
                    // The card is sized from the pure `row_shape`; assert the live rows match it
                    // (count + separator positions) so the geometry can't drift from what's drawn.
                    debug_assert_eq!(
                        rows.iter().map(DetailRow::spec).collect::<Vec<_>>(),
                        owner.row_shape(self.layout().owner_device_count(owner)),
                        "rows() must match row_shape() for correct card sizing"
                    );
                    let row_runs = rows
                        .into_iter()
                        .map(|r| -> anyhow::Result<DetailRowRun> {
                            let r = match r {
                                DetailRow::Item(row) => row,
                                // A name label is an ordinary regular-weight menu item with
                                // nothing else in it (`brightness.js:14-15`).
                                DetailRow::Label(label) => ItemRow {
                                    label,
                                    icons: Vec::new(),
                                    action: PopoverAction::Consumed,
                                    separator_before: false,
                                    selected: false,
                                    trailing: None,
                                    placeholder: false,
                                },
                                // A slider row has no text at all; only its value is baked.
                                DetailRow::Slider { value, .. } => {
                                    return Ok(DetailRowRun::Slider { value })
                                }
                            };
                            // The placeholder is `.bt-menu-placeholder` = `%title_4` (13pt/700,
                            // centered, `_quick-settings.scss:227-232`); ordinary rows are
                            // regular-weight menu items.
                            let style = if r.placeholder {
                                TextStyle::new(DETAIL_PLACEHOLDER_PT).bold()
                            } else {
                                TextStyle::new(DETAIL_ROW_PT)
                            };
                            Ok(DetailRowRun::Text(TextRun {
                                label: shaper.shape(&r.label, style)?,
                                trailing: r
                                    .trailing
                                    .map(|t| shaper.shape(&t, TextStyle::new(DETAIL_ROW_PT)))
                                    .transpose()?,
                                placeholder: r.placeholder,
                                has_icon: !r.icons.is_empty(),
                            }))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok((title_run, row_runs))
                })
                .transpose()?;
            (label_runs, subtitle_runs, pill_run, detail_runs)
        };

        widget::bake_uncached_sized(renderer, phys, |frame| {
            let mut p = Painter::new(frame, scale, phys);
            // Transparent bg: the shared popover chrome (`PanelPopover::render`) draws the
            // `.popup-menu-content` box fill (`$bg_color`) behind this content. Tiles and cards
            // are drawn on top; wherever this texture is transparent (outer corners, tile pill-
            // corner gaps) the chrome bg shows through — the same `$bg_color` as before.
            p.clear(style::TRANSPARENT)?;

            for (i, item) in self.grid().into_iter().enumerate() {
                let rect = tile_rect(i, layout);
                let on = item.is_on(
                    self.toggles,
                    self.network,
                    self.airplane,
                    &self.power,
                    self.bluetooth.powered,
                );
                let bg = if on { self.accent } else { TILE_OFF };
                // gnome-shell quick toggles use `$forced_circular_radius` → pill-shaped; a
                // half-height radius clamps to the pill in `sdf_rect.frag`. The cut corners fall
                // back to the chrome's menu bg behind this texture.
                p.fill_rounded(rect, rect.size.h / 2., bg)?;
                if self.hovered == Some(QsHover::Tile(i)) {
                    p.fill_rounded(rect, rect.size.h / 2., style::HOVER_WASH)?;
                }

                // A menu tile's arrow-half is separated from the body by a 1px divider
                // (`.quick-toggle-separator`); v1 keeps one pill background and marks the split
                // with the divider + the arrow icon (the split-radius look is a later cosmetic).
                if let Some(arrow) = tile_arrow_rect(i, item, layout) {
                    let sep = Rectangle::new(
                        Point::from((arrow.loc.x - SEPARATOR_W, arrow.loc.y + arrow.size.h * 0.2)),
                        Size::from((SEPARATOR_W, arrow.size.h * 0.6)),
                    );
                    p.fill_rounded(sep, 0., SEPARATOR_COLOR)?;
                }

                let fg = if on { FG_ON } else { FG_OFF };
                let label_x = rect.loc.x + TILE_ICON_INSET + TILE_ICON + 8.;
                let center_y = rect.loc.y + rect.size.h / 2.;
                let run = &label_runs[i];
                // Clip the label to the toggle-body so a long name can't run under the arrow
                // (gnome-shell ellipsizes; clipping is the minimal faithful bound).
                let clip = tile_body_rect(i, item, layout);
                match &subtitle_runs[i] {
                    // Two-line tile (Power Mode): title above center, subtitle below.
                    Some(sub) => {
                        p.text_clipped(
                            run,
                            Point::from((label_x, center_y - SUBTITLE_GAP)),
                            Align::LEFT_MIDDLE,
                            fg,
                            clip,
                        )?;
                        p.text_clipped(
                            sub,
                            Point::from((label_x, center_y + SUBTITLE_GAP)),
                            Align::LEFT_MIDDLE,
                            fg,
                            clip,
                        )?;
                    }
                    // Single-line tile: the title, vertically centered.
                    None => {
                        p.text_clipped(
                            run,
                            Point::from((label_x, center_y)),
                            Align::LEFT_MIDDLE,
                            fg,
                            clip,
                        )?;
                    }
                }
            }

            // The battery pill: a filled slab (its icon composites on top) with the
            // percentage after it.
            if let (Some(pill), Some(run)) = (pill_rect(self.has_pill()), &pill_run) {
                p.fill_rounded(pill, pill.size.h / 2., TILE_OFF)?;
                if self.hovered == Some(QsHover::Pill) {
                    p.fill_rounded(pill, pill.size.h / 2., style::HOVER_WASH)?;
                }
                let label_x = pill.loc.x + PILL_ICON_INSET + SYS_ICON + 8.;
                let label_cy = pill.loc.y + pill.size.h / 2.;
                p.text(
                    run,
                    Point::from((label_x, label_cy)),
                    Align::LEFT_MIDDLE,
                    FG_OFF,
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
                p.fill_rounded(disc, SYS_HIT / 2., TILE_OFF)?;
                if self.hovered == Some(QsHover::Sys(button)) {
                    p.fill_rounded(disc, SYS_HIT / 2., style::HOVER_WASH)?;
                }
            }

            // Each present slider: mute-button disc, the trough, its accent-filled portion, and the
            // round handle (`.quick-slider` + `_slider.scss`). The level icon composites on top
            // afterwards. Output and mic share the geometry; only the level/mute source differs.
            for slider in SLIDERS {
                if !layout.sliders.present(slider) {
                    continue;
                }
                let (volume, muted) = match slider {
                    Slider::Output => {
                        let a = self.audio.unwrap_or_default();
                        (a.volume, a.muted)
                    }
                    Slider::Mic => (self.mic.volume, self.mic.muted),
                    // Brightness has no mute, so the fill is always the accent colour.
                    Slider::Brightness => (self.brightness.global.unwrap_or_default(), false),
                };
                let disc = slider_icon_rect(slider, layout);
                p.fill_rounded(disc, SLIDER_H / 2., TILE_OFF)?;
                if self.hovered == Some(QsHover::SliderIcon(slider)) {
                    p.fill_rounded(disc, SLIDER_H / 2., style::HOVER_WASH)?;
                }

                let fill_color = if muted { SLIDER_TROUGH_BG } else { self.accent };
                paint_slider(
                    &mut p,
                    slider_track_rect(slider, layout),
                    volume,
                    fill_color,
                )?;
            }

            // The open detail view: the `%card` background, its header title (the header icon
            // composites on top in `render`), then the row labels. Row icons, if any, also
            // composite on top.
            if let (Some(card), Some((title_run, row_runs))) = (detail_rect(layout), &detail_runs) {
                p.fill_rounded(card, DETAIL_RADIUS, CARD_BG)?;

                // The `.header .icon` highlighted circular pill, behind the (separately
                // composited) header icon glyph.
                let pill_cx = card.loc.x + DETAIL_HEADER_INSET + DETAIL_HEADER_PILL / 2.;
                let pill_cy = card.loc.y + DETAIL_PAD + DETAIL_HEADER_H / 2.;
                let pill = Rectangle::new(
                    Point::from((
                        pill_cx - DETAIL_HEADER_PILL / 2.,
                        pill_cy - DETAIL_HEADER_PILL / 2.,
                    )),
                    Size::from((DETAIL_HEADER_PILL, DETAIL_HEADER_PILL)),
                );
                p.fill_rounded(pill, DETAIL_HEADER_PILL / 2., DETAIL_HEADER_PILL_BG)?;

                let title_x =
                    card.loc.x + DETAIL_HEADER_INSET + DETAIL_HEADER_PILL + DETAIL_HEADER_GAP;
                let title_cy = card.loc.y + DETAIL_PAD + DETAIL_HEADER_H / 2.;
                p.text_clipped(
                    title_run,
                    Point::from((title_x, title_cy)),
                    Align::LEFT_MIDDLE,
                    FG_OFF,
                    card,
                )?;

                // The group-separator rules (`.popup-separator-menu-item`): one 1px rule per row
                // that opens a new group (the shutdown menu's machine-power vs session split).
                let sep_shape = self
                    .expanded
                    .map(|o| o.row_shape(layout.owner_device_count(o)))
                    .unwrap_or_default();
                for (k, row_run) in row_runs.iter().enumerate() {
                    let Some(rrect) = detail_row_rect(k, layout) else {
                        continue;
                    };
                    // A card slider row: the shared slider body, inset like a `%menuitem`. No
                    // separator, no hover (`brightness.js:29` is a non-reactive item), no text.
                    let row_run = match row_run {
                        DetailRowRun::Text(run) => run,
                        DetailRowRun::Slider { value } => {
                            paint_slider(
                                &mut p,
                                detail_slider_track_rect(rrect),
                                *value,
                                self.accent,
                            )?;
                            continue;
                        }
                    };
                    let run = &row_run.label;
                    // The group-separator rule, centered in the extra gap above a group-opening
                    // row. The same `$borders_color` + crisp `Painter::hairline` as the calendar
                    // column separator — but this bakes onto the *opaque* card, where the clear
                    // would punch a translucent hole, so pre-blend the border over the card
                    // (`style::over`) to lay the identical line as an opaque color.
                    if k > 0 && sep_shape.get(k).is_some_and(|s| s.separator_before) {
                        let line_cy = rrect.loc.y - (DETAIL_ROW_GAP + DETAIL_SEP_EXTRA) / 2.;
                        // The separator is itself a `.popup-separator-menu-item`, so its rule is
                        // inset by the `%menuitem` horizontal padding (`$base_padding*2`, our
                        // `DETAIL_ROW_INSET`) on top of the card pad — i.e. its ends align with the
                        // row labels, not the card edge (`_common.scss:135`, `_popovers.scss:113`).
                        let inset = DETAIL_PAD + DETAIL_ROW_INSET;
                        p.hairline(
                            Rectangle::new(
                                Point::from((card.loc.x + inset, line_cy)),
                                Size::from((card.size.w - 2. * inset, 1.)),
                            ),
                            style::over(CARD_BG, style::BORDERS),
                        )?;
                    }
                    // A hovered picker row: a faint rounded fill (it has no base
                    // bg) behind the label, matching GNOME's flat menu-item hover.
                    if self.hovered == Some(QsHover::DetailRow(k)) {
                        p.fill_rounded(rrect, 8., style::HOVER_WASH)?;
                    }
                    let label_cy = rrect.loc.y + rrect.size.h / 2.;
                    // The bluetooth placeholder line: centered bold text, nothing else in the row
                    // (`.bt-menu-placeholder`, `_quick-settings.scss:227-232`).
                    if row_run.placeholder {
                        p.text_clipped(
                            run,
                            Point::from((rrect.loc.x + rrect.size.w / 2., label_cy)),
                            Align::CENTER,
                            FG_OFF,
                            rrect,
                        )?;
                        continue;
                    }
                    let label_x = if row_run.has_icon {
                        rrect.loc.x + DETAIL_ROW_INSET + TILE_ICON + 8.
                    } else {
                        rrect.loc.x + DETAIL_ROW_INSET
                    };
                    // The trailing sublabel (Connect/Disconnect, `.device-subtitle` = fg@50%,
                    // `_quick-settings.scss:234`), right-aligned where the check zone sits.
                    let mut trailing_w = TILE_ICON;
                    if let Some(trailing) = &row_run.trailing {
                        trailing_w = f64::from(trailing.ink_bounds().2) / scale;
                        p.text(
                            trailing,
                            Point::from((rrect.loc.x + rrect.size.w - DETAIL_ROW_INSET, label_cy)),
                            Align::RIGHT_MIDDLE,
                            DETAIL_SUBTITLE_FG,
                        )?;
                    }
                    // Reserve the trailing zone (the check icon, or the sublabel's width) on every
                    // row so a long label (e.g. a verbose HDMI sink description or device alias)
                    // is clipped before it, not drawn under it.
                    let mut label_clip = rrect;
                    label_clip.size.w =
                        (label_clip.size.w - (DETAIL_ROW_INSET + trailing_w + 8.)).max(0.);
                    p.text_clipped(
                        run,
                        Point::from((label_x, label_cy)),
                        Align::LEFT_MIDDLE,
                        FG_OFF,
                        label_clip,
                    )?;
                }
            }

            Ok(())
        })
    }
}

/// The menu's logical width: two tile columns plus padding.
fn menu_w() -> f64 {
    PAD * 2. + COLS as f64 * tile_w() + (COLS as f64 - 1.) * TILE_GAP
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

/// A slider's drawn body inside `track`: the trough, its value fill and the round handle
/// (`.quick-slider` + `_slider.scss`). Shared by the top-level slider rows and the brightness
/// card's per-monitor rows, so the two can't drift.
fn paint_slider(
    p: &mut Painter,
    track: Rectangle<f64, Logical>,
    value: f64,
    fill_color: [f32; 4],
) -> anyhow::Result<()> {
    let cy = track.loc.y + track.size.h / 2.;
    let trough = Rectangle::new(
        Point::from((track.loc.x, cy - SLIDER_TROUGH / 2.)),
        Size::from((track.size.w, SLIDER_TROUGH)),
    );
    p.fill_rounded(trough, SLIDER_TROUGH / 2., SLIDER_TROUGH_BG)?;

    // The fill's width comes from the two extremities, never from a rounded size (rounding loc
    // and size apart doubles the error on the far edge).
    let handle_cx = handle_x_on_track(track, value);
    let fill = Rectangle::new(
        trough.loc,
        Size::from(((handle_cx - track.loc.x).max(0.), SLIDER_TROUGH)),
    );
    p.fill_rounded(fill, SLIDER_TROUGH / 2., fill_color)?;

    let handle = Rectangle::new(
        Point::from((handle_cx - SLIDER_HANDLE / 2., cy - SLIDER_HANDLE / 2.)),
        Size::from((SLIDER_HANDLE, SLIDER_HANDLE)),
    );
    p.fill_rounded(handle, SLIDER_HANDLE / 2., FG_OFF)
}

/// The track band of a slider row inside a detail card: the row minus the `%menuitem` inset.
/// There is no icon disc and no arrow to make room for — gnome-shell's card slider is a bare
/// `Slider` in a `slider-bin` filling the item (`brightness.js:17-31`).
fn detail_slider_track_rect(row: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((row.loc.x + DETAIL_ROW_INSET, row.loc.y)),
        Size::from(((row.size.w - 2. * DETAIL_ROW_INSET).max(0.), row.size.h)),
    )
}

/// Perceptual value `0..=1` for a pointer x on any track band. The usable span is inset by half a
/// handle at each end, so the handle center never leaves the band.
fn volume_from_track_x(track: Rectangle<f64, Logical>, x: f64) -> f64 {
    let left = track.loc.x + SLIDER_HANDLE / 2.;
    let right = track.loc.x + track.size.w - SLIDER_HANDLE / 2.;
    ((x - left) / (right - left)).clamp(0.0, 1.0)
}

/// The handle-center x for a value on any track band — the inverse of [`volume_from_track_x`].
fn handle_x_on_track(track: Rectangle<f64, Logical>, volume: f64) -> f64 {
    let left = track.loc.x + SLIDER_HANDLE / 2.;
    let right = track.loc.x + track.size.w - SLIDER_HANDLE / 2.;
    left + volume.clamp(0.0, 1.0) * (right - left)
}

/// Perceptual volume `0..=1` for a pointer x on a top-level slider's track.
fn volume_from_x(sl: Slider, x: f64, layout: Layout) -> f64 {
    volume_from_track_x(slider_track_rect(sl, layout), x)
}

/// The rectangle of tile `i` (row-major), menu-local logical. The grid sits below the top system
/// row (and the volume slider when a sink is present); rows below an open detail view's owner
/// shift down by the card block.
fn tile_rect(i: usize, layout: Layout) -> Rectangle<f64, Logical> {
    let row = (i / COLS) as f64;
    let col = (i % COLS) as f64;
    let x = PAD + col * (tile_w() + TILE_GAP);
    let y = grid_top(layout.sliders) + row * (TILE_H + TILE_GAP);
    Rectangle::new(
        Point::from((x, y + layout.shift_below(y))),
        Size::from((tile_w(), TILE_H)),
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
    let insert_y = owner.anchor_row_bottom(layout);
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
    for (j, spec) in shape.iter().enumerate() {
        if j > 0 {
            y += DETAIL_ROW_GAP;
        }
        if spec.separator_before {
            y += DETAIL_SEP_EXTRA;
        }
        if j == k {
            return Some(Rectangle::new(
                Point::from((card.loc.x + DETAIL_PAD, y)),
                Size::from((card.size.w - 2. * DETAIL_PAD, spec.kind.height())),
            ));
        }
        y += spec.kind.height();
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

#[cfg(test)]
mod tests {
    use smithay::utils::Buffer as BufferCoord;

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
                brightness: false,
            },
            expanded: None,
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            bt_device_count: 0,
            monitor_count: 0,
            show_bluetooth: false,
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
                BluetoothStatus::default(),
                BluetoothRfkill::default(),
                None,
                audio,
                SinkList::default(),
                crate::audio::AudioCards::default(),
                false,
                MicStatus::default(),
                SourceList::default(),
                crate::brightness::BrightnessView::default(),
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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

        // A content change that shifts the grid (airplane tile appears) clears a
        // stale hover, so its wash can't land on a now-different tile.
        qs.pointer_hover(Some(center(tile_rect(1, lay(false)))));
        assert!(qs.hovered.is_some());
        assert!(qs.set_airplane(AirplaneStatus {
            active: false,
            show: true,
        }));
        assert_eq!(
            qs.hovered, None,
            "an external content change clears the stale hover"
        );
    }

    /// The Network tile (grid cell 0) reads as "on" when connected and, clicked,
    /// opens network settings without flipping any local toggle state.
    #[test]
    fn network_tile_reflects_state_and_opens_settings() {
        let off = AirplaneStatus::default();
        let np = PowerProfileStatus::default();
        assert!(GridTile::Network.is_on(
            QuickToggles::default(),
            NetworkStatus::Wired,
            off,
            &np,
            false
        ));
        assert!(GridTile::Network.is_on(
            QuickToggles::default(),
            NetworkStatus::Wireless(60),
            off,
            &np,
            false
        ));
        assert!(!GridTile::Network.is_on(
            QuickToggles::default(),
            NetworkStatus::Offline,
            off,
            &np,
            false
        ));

        let mut qs = QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            Some(battery(79.)),
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem, Texture as _};

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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            Some(battery(79.)),
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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

        // The tile's corner is cut to the pill: a deep-corner pixel is transparent (the shared
        // popover chrome's menu bg shows through behind this content texture), not the accent —
        // proving `render_rounded_rect` rounded the tile in the offscreen.
        let kx = (r0.loc.x + 2.) as i32;
        let ky = (r0.loc.y + 2.) as i32;
        let k = ((ky * size.w + kx) * 4) as usize;
        let corner = [pixels[k], pixels[k + 1], pixels[k + 2], pixels[k + 3]];
        assert!(
            corner[3] < 40,
            "the active tile's corner must be cut to transparent (chrome shows through), got {corner:?}"
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

    /// The `ItemRow` behind an ordinary card row (the picker/settings rows every pre-brightness
    /// card is made of); panics on a label/slider row, which is what a test asserting an action
    /// or a label wants.
    fn item(row: &DetailRow) -> &ItemRow {
        row.item().expect("an ordinary item row")
    }

    fn qs(network: NetworkStatus, audio: Option<AudioStatus>) -> QuickSettings {
        QuickSettings::new(
            QuickToggles::default(),
            network,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            audio,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
                    card: None,
                    form_factor: None,
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
                    card: None,
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

    /// A brightness view with `n` backlit monitors, the global scale at `value`.
    fn brightness_view(n: usize, value: f64) -> crate::brightness::BrightnessView {
        crate::brightness::BrightnessView {
            global: (n > 0).then_some(value),
            monitors: (0..n)
                .map(|i| crate::brightness::MonitorView {
                    connector: format!("eDP-{i}"),
                    name: format!("Display {i}"),
                    value,
                })
                .collect(),
        }
    }

    /// A menu with a live brightness slider (one backlit panel) and no audio.
    fn qs_with_brightness(value: f64) -> QuickSettings {
        let mut q = qs(NetworkStatus::Wired, None);
        q.brightness = brightness_view(1, value);
        q
    }

    /// The brightness slider exists only when some monitor has a backlight
    /// (`brightness.js:59-60` gates on the global scale), and stacks BELOW both volume sliders --
    /// gnome-shell adds the items in the order output, input, brightness (`panel.js:366-373`).
    #[test]
    fn brightness_slider_shows_with_a_backlight_and_stacks_last() {
        let none = qs(NetworkStatus::Wired, None);
        assert!(!none.layout().sliders.brightness);

        // Alone, it takes the top slot (a desktop with no bound sink and no mic).
        let only = qs_with_brightness(0.5);
        assert!(only.layout().sliders.brightness);
        assert_eq!(
            slider_row_rect(Slider::Brightness, only.layout()).loc.y,
            slider_row_rect(
                Slider::Output,
                qs(NetworkStatus::Wired, Some(AudioStatus::default())).layout(),
            )
            .loc
            .y,
        );

        // With both volume sliders up it is the third row, below the mic.
        let mut all = qs_with_sources(1);
        all.brightness = brightness_view(1, 0.5);
        let layout = all.layout();
        let out = slider_row_rect(Slider::Output, layout).loc.y;
        let mic = slider_row_rect(Slider::Mic, layout).loc.y;
        let bright = slider_row_rect(Slider::Brightness, layout).loc.y;
        assert!(out < mic && mic < bright);
        assert_eq!(layout.sliders.count(), 3);

        // Hiding the mic closes the gap rather than leaving a hole.
        all.mic = MicStatus::default();
        let layout = all.layout();
        assert_eq!(
            slider_row_rect(Slider::Brightness, layout).loc.y,
            slider_row_rect(Slider::Mic, layout).loc.y,
        );
    }

    /// The brightness icon is decoration, not a button: `QuickSlider`'s icon reactivity is opt-in
    /// (`quickSettings.js:290-311`) and brightness does not opt in, unlike the two mute buttons.
    #[test]
    fn the_brightness_icon_is_not_a_button() {
        let mut q = qs_with_brightness(0.5);
        let layout = q.layout();
        let icon = center(slider_icon_rect(Slider::Brightness, layout));

        // No hover highlight...
        q.pointer_hover(Some(icon));
        assert_eq!(q.hovered, None);
        // ... and a click on it does nothing but land.
        assert_eq!(q.pointer_click(icon), PopoverAction::Consumed);
        assert!(q.sliding.is_none());
    }

    /// Dragging the track reports the new global value and moves the handle optimistically, so it
    /// tracks the pointer instead of waiting for the udev echo.
    #[test]
    fn dragging_brightness_sets_the_global_scale() {
        let mut q = qs_with_brightness(0.);
        let layout = q.layout();
        let track = slider_track_rect(Slider::Brightness, layout);

        let action = q.pointer_click(center(track));
        let PopoverAction::SetBrightness(value) = action else {
            panic!("expected SetBrightness, got {action:?}");
        };
        assert!((value - 0.5).abs() < 0.01);
        assert!(matches!(
            q.sliding,
            Some((SliderId::Top(Slider::Brightness), _))
        ));
        assert!((q.brightness.global.unwrap() - value).abs() < 1e-9);

        // The lagging echo is adopted, but the dragged global value is held at the pointer, so
        // the handle does not snap back. (Adopting the rest matters: a snapshot dropped mid-drag
        // is never re-pushed, so a monitor that vanished would leave a phantom row behind.)
        let mut echo = brightness_view(2, 0.);
        echo.global = Some(0.);
        assert!(q.set_brightness(echo));
        assert!((q.brightness.global.unwrap() - value).abs() < 1e-9);
        assert_eq!(
            q.brightness.monitors.len(),
            2,
            "the structural change lands"
        );
        assert_eq!(q.brightness.monitors[0].value, 0.);

        // But the backlight going away cancels the drag before the slider hides.
        assert!(q.set_brightness(crate::brightness::BrightnessView::default()));
        assert!(q.sliding.is_none());
        assert!(!q.layout().sliders.brightness);
    }

    /// A menu whose brightness card is open over `n` monitors.
    fn qs_with_card(n: usize) -> QuickSettings {
        let mut q = qs(NetworkStatus::Wired, None);
        q.brightness = brightness_view(n, 0.5);
        q.expanded = Some(DetailOwner::Brightness);
        q
    }

    /// The picker arrow appears only with more than one scale (`menuEnabled =
    /// this._manager.scales.length > 1`, `brightness.js:61`) -- and `scales` is the per-monitor
    /// scales, the global one excluded (`brightnessManager.js:104-106`).
    #[test]
    fn the_brightness_arrow_needs_more_than_one_monitor() {
        let one = qs_with_brightness(0.5);
        assert!(slider_arrow_rect(Slider::Brightness, one.layout()).is_none());

        let mut two = qs_with_brightness(0.5);
        two.brightness = brightness_view(2, 0.5);
        let arrow = slider_arrow_rect(Slider::Brightness, two.layout())
            .expect("an arrow with two backlit monitors");

        // Clicking it opens (and re-clicking closes) the card, like every other picker.
        assert_eq!(two.pointer_click(center(arrow)), PopoverAction::Consumed);
        assert_eq!(two.expanded, Some(DetailOwner::Brightness));
        let arrow = slider_arrow_rect(Slider::Brightness, two.layout()).unwrap();
        assert_eq!(two.pointer_click(center(arrow)), PopoverAction::Consumed);
        assert_eq!(two.expanded, None);
    }

    /// The card is a (name label, slider) pair per monitor and nothing else: no separator, no
    /// settings row (`brightness.js:47-49` adds only the header and the slider section, and
    /// `addSlider` `:13-34` adds exactly those two items).
    #[test]
    fn the_brightness_card_is_label_slider_pairs() {
        let q = qs_with_card(2);
        let rows = q.detail_rows(DetailOwner::Brightness);
        assert_eq!(rows.len(), 4);
        assert!(matches!(&rows[0], DetailRow::Label(n) if n == "Display 0"));
        assert!(matches!(&rows[1], DetailRow::Slider { connector, .. } if connector == "eDP-0"));
        assert!(matches!(&rows[2], DetailRow::Label(n) if n == "Display 1"));
        assert!(matches!(&rows[3], DetailRow::Slider { connector, .. } if connector == "eDP-1"));

        // The live rows must match the pure shape the card was sized from.
        assert_eq!(
            rows.iter().map(DetailRow::spec).collect::<Vec<_>>(),
            DetailOwner::Brightness.row_shape(2),
        );
        // No separators anywhere -- unlike every other card, which ends in one.
        assert!(DetailOwner::Brightness
            .row_shape(2)
            .iter()
            .all(|s| !s.separator_before));

        // Rows stack top-down without overlapping, and the card contains them all.
        let layout = q.layout();
        let card = detail_rect(layout).expect("an open card");
        let rects: Vec<_> = (0..rows.len())
            .map(|k| detail_row_rect(k, layout).expect("a row rect"))
            .collect();
        for pair in rects.windows(2) {
            assert!(pair[0].loc.y + pair[0].size.h <= pair[1].loc.y);
        }
        let last = rects.last().unwrap();
        assert!(last.loc.y + last.size.h <= card.loc.y + card.size.h);
    }

    /// Both card rows are `reactive: false` in gnome-shell (`brightness.js:14,29`), so neither
    /// ever takes the menu-item hover highlight -- only the slider inside is interactive.
    #[test]
    fn brightness_card_rows_never_highlight() {
        let mut q = qs_with_card(2);
        let layout = q.layout();
        for k in 0..4 {
            let rect = detail_row_rect(k, layout).unwrap();
            q.pointer_hover(Some(center(rect)));
            assert_eq!(q.hovered, None, "row {k} must not highlight");
        }
    }

    /// Dragging a card row's track reports that monitor's connector, and the track spans the row
    /// minus the menu-item inset (no icon disc, no arrow -- the card slider is a bare slider bin).
    #[test]
    fn dragging_a_card_row_sets_that_monitor() {
        let mut q = qs_with_card(2);
        let layout = q.layout();
        // Row 3 is the second monitor's slider.
        let row = detail_row_rect(3, layout).unwrap();
        let track = detail_slider_track_rect(row);
        assert!(track.loc.x > row.loc.x && track.size.w < row.size.w);

        let action = q.pointer_click(center(track));
        let PopoverAction::SetMonitorBrightness(connector, value) = action else {
            panic!("expected SetMonitorBrightness, got {action:?}");
        };
        assert_eq!(connector, "eDP-1");
        assert!((value - 0.5).abs() < 0.01);
        assert_eq!(q.sliding, Some((SliderId::Monitor("eDP-1".into()), 2)));
        // Optimistic: only that monitor moved.
        assert!((q.brightness.monitors[1].value - value).abs() < 1e-9);
        assert_eq!(q.brightness.monitors[0].value, 0.5);

        // The track's ends map to the full range.
        assert_eq!(volume_from_track_x(track, track.loc.x), 0.);
        assert_eq!(volume_from_track_x(track, track.loc.x + track.size.w), 1.);

        // Dragging continues to the same monitor, and releasing clears the drag.
        let left = Point::from((track.loc.x, center(track).y));
        assert_eq!(
            q.pointer_drag(left),
            Some(PopoverAction::SetMonitorBrightness("eDP-1".into(), 0.)),
        );
        assert!(!q.end_drag());
        assert!(q.sliding.is_none());
    }

    /// A card drag adopts fresh snapshots (gnome-shell pulls the GLOBAL scale to the max while a
    /// monitor scale is dragged, `brightnessManager.js:203-228`) while holding the dragged row at
    /// the pointer -- and the monitor going away cancels the drag.
    #[test]
    fn a_card_drag_holds_only_its_own_row() {
        let mut q = qs_with_card(2);
        let layout = q.layout();
        let track = detail_slider_track_rect(detail_row_rect(1, layout).unwrap());
        q.pointer_click(center(track));
        let dragged = q.brightness.monitors[0].value;

        // A snapshot arrives with everything moved: the other monitor and the global scale follow
        // it, the dragged row does not.
        let mut view = brightness_view(2, 0.2);
        view.global = Some(0.9);
        assert!(q.set_brightness(view));
        assert_eq!(q.brightness.global, Some(0.9));
        assert_eq!(q.brightness.monitors[1].value, 0.2);
        assert!((q.brightness.monitors[0].value - dragged).abs() < 1e-9);

        // The dragged monitor unplugs: the drag is cancelled, and dropping to one scale closes
        // the card (its arrow gate is gone).
        let mut view = brightness_view(2, 0.5);
        view.monitors.remove(0);
        assert!(q.set_brightness(view));
        assert!(q.sliding.is_none());
        assert_eq!(q.expanded, None);
    }

    /// Only the track band of a card row is reactive: in gnome-shell the slider's menu item is
    /// `reactive: false` and just holds the `slider-bin` (`brightness.js:23,29`). Without this a
    /// click a few px left of the track would clamp to 0 -- i.e. slam that monitor dark.
    #[test]
    fn a_near_miss_on_a_card_slider_does_nothing() {
        let mut q = qs_with_card(2);
        let layout = q.layout();
        let row = detail_row_rect(1, layout).unwrap();
        let track = detail_slider_track_rect(row);

        // In the inset strip left of the track, still inside the row.
        let miss = Point::from((row.loc.x + 1., row.loc.y + row.size.h / 2.));
        assert!(row.contains(miss) && !track.contains(miss));
        assert_eq!(q.pointer_click(miss), PopoverAction::Consumed);
        assert!(q.sliding.is_none());
        assert_eq!(
            q.brightness.monitors[0].value, 0.5,
            "unchanged by a near miss"
        );
    }

    /// A drag holds only the dragged VALUE; structural changes in a snapshot are always adopted.
    /// `set_brightness` is only called on brightness events, so a rejected snapshot is never
    /// re-pushed -- a monitor that unplugged mid-drag would leave a phantom row and a live picker
    /// arrow behind indefinitely.
    #[test]
    fn a_top_slider_drag_still_adopts_structural_changes() {
        let mut q = qs_with_brightness(0.);
        q.brightness = brightness_view(2, 0.5);
        let layout = q.layout();
        q.pointer_click(center(slider_track_rect(Slider::Brightness, layout)));
        let dragged = q.brightness.global.unwrap();
        assert!(slider_arrow_rect(Slider::Brightness, q.layout()).is_some());

        // The dock unplugs mid-drag.
        let mut view = brightness_view(1, 0.5);
        view.global = Some(0.1);
        assert!(q.set_brightness(view));
        assert_eq!(q.brightness.monitors.len(), 1);
        assert!((q.brightness.global.unwrap() - dragged).abs() < 1e-9);

        // The arrow is still drawn from the count frozen at drag start -- that freeze is what
        // stops the track resizing under a stationary pointer. Releasing unfreezes it, and the
        // arrow goes with the monitor rather than opening a card of phantom rows.
        assert!(slider_arrow_rect(Slider::Brightness, q.layout()).is_some());
        assert!(q.end_drag(), "the frozen count no longer matches");
        assert!(slider_arrow_rect(Slider::Brightness, q.layout()).is_none());
    }

    /// A card drag cannot outlive its card: with the card closed there is nothing on screen to
    /// explain why incoming snapshots keep getting overridden.
    #[test]
    fn closing_the_card_cancels_a_card_drag() {
        let mut q = qs_with_card(2);
        let layout = q.layout();
        let track = detail_slider_track_rect(detail_row_rect(1, layout).unwrap());
        q.pointer_click(center(track));
        assert!(matches!(q.sliding, Some((SliderId::Monitor(_), _))));

        // The OTHER monitor unplugs: the dragged one is still there, but one scale means no
        // arrow, so the card closes -- and the drag goes with it.
        let mut view = brightness_view(2, 0.5);
        view.monitors.pop();
        assert!(q.set_brightness(view));
        assert_eq!(q.expanded, None);
        assert!(q.sliding.is_none());
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

        let rows = q.detail_rows(DetailOwner::Input);
        assert_eq!(rows.len(), 4, "3 sources + Sound Settings");
        assert!(item(&rows[0]).selected && !item(&rows[1]).selected);
        assert_eq!(item(&rows[3]).label, "Sound Settings");
        match q.pointer_click(center(detail_row_rect(1, q.layout()).unwrap())) {
            PopoverAction::SetInputDevice(crate::audio::AudioDeviceKey::Node(name)) => {
                assert_eq!(name, "source1")
            }
            other => panic!("expected SetInputDevice, got {other:?}"),
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
        assert!(matches!(q.sliding, Some((SliderId::Top(Slider::Mic), _))));
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
        assert!(matches!(
            q.sliding,
            Some((SliderId::Top(Slider::Output), _))
        ));
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
        assert_eq!(
            arrow.loc.x + arrow.size.w,
            tile_rect(ni, l).loc.x + tile_w()
        );
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
            tile_rect(1, qs.layout()).loc.x + tile_w() / 2.,
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
            [0, 0, 0],
        );

        // Appended as the 5th tile at the constant power_profile_index (4).
        let tiles = qs.grid();
        assert_eq!(tiles.len(), 5);
        assert_eq!(power_profile_index(false), 4);
        assert_eq!(tiles[power_profile_index(false)], GridTile::PowerProfile);

        // Two-line tile: static "Power Mode" title, active profile as the subtitle.
        assert_eq!(
            GridTile::PowerProfile.label(NetworkStatus::Wired),
            "Power Mode"
        );
        assert_eq!(
            GridTile::PowerProfile
                .subtitle(&qs.power, &qs.bluetooth)
                .as_deref(),
            Some("Performance")
        );
        // On because the active profile isn't Balanced.
        assert!(GridTile::PowerProfile.is_on(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            &qs.power,
            false,
        ));

        // Body click returns the toggle action; no arrow yet, so the whole tile is body.
        let action = qs.pointer_click(center(tile_rect(power_profile_index(false), qs.layout())));
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
            BluetoothStatus::default(),
            BluetoothRfkill::default(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
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
        let ppi = power_profile_index(false);

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
        let rows = qs.detail_rows(DetailOwner::PowerProfile);
        assert_eq!(rows.len(), 4);
        assert_eq!(item(&rows[0]).label, "Performance");
        assert_eq!(item(&rows[2]).label, "Power Saver");
        assert!(item(&rows[0]).selected && !item(&rows[1]).selected);
        assert!(
            item(&rows[3]).separator_before,
            "Power Settings past a separator"
        );

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
        let ppi = power_profile_index(false);
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
                brightness: false,
            },
            expanded: None,
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            bt_device_count: 0,
            monitor_count: 0,
            show_bluetooth: false,
            drag: None,
            grid_len: BASE_GRID.len(),
        };
        let expanded = Layout {
            sliders: Sliders {
                output: false,
                mic: false,
                brightness: false,
            },
            expanded: Some(DetailOwner::Network),
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            bt_device_count: 0,
            monitor_count: 0,
            show_bluetooth: false,
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
                        brightness: false,
                    },
                    expanded: Some(owner),
                    sink_count: 0,
                    source_count: 0,
                    profile_count: 0,
                    bt_device_count: 0,
                    monitor_count: 0,
                    show_bluetooth: false,
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
        let rows = qs.detail_rows(DetailOwner::Power);
        assert_eq!(rows.len(), 4);
        assert!(
            item(&rows[3]).separator_before,
            "Log Out starts the session group"
        );

        // The "Power Off…" row (index 2) spawns the shutdown command.
        let row = detail_row_rect(2, qs.layout()).expect("the power-off row");
        match qs.pointer_click(center(row)) {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-session-quit", "--power-off"]),
            other => panic!("expected the power-off spawn, got {other:?}"),
        }
    }

    /// Baking the open shutdown submenu exercises the new header icon pill and the group-separator
    /// rule; a no-Vulkan environment skips. Under `NIRI_VK_VALIDATION` this checks those draw
    /// calls against the spec (a bare geometry test can't see the pill/rule at all).
    #[test]
    fn shutdown_submenu_bakes_with_pill_and_separator() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "skipping shutdown_submenu_bakes_with_pill_and_separator: no Vulkan ({e})"
                );
                return;
            }
        };
        let mut qs = qs(NetworkStatus::Wired, None);
        qs.pointer_click(center(sys_rect(SysButton::Power, false)));
        assert_eq!(qs.expanded, Some(DetailOwner::Power));
        for scale in [1.0, 2.0] {
            let tex = qs.draw(&mut vk, scale).expect("shutdown submenu bakes");
            let size = smithay::backend::renderer::Texture::size(&tex);
            assert!(size.w > 0 && size.h > 0, "non-empty at scale {scale}");
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
                brightness: false,
            },
            expanded: None,
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            bt_device_count: 0,
            monitor_count: 0,
            show_bluetooth: false,
            drag: None,
            grid_len: BASE_GRID.len(),
        };
        let expanded = Layout {
            sliders: Sliders {
                output: true,
                mic: false,
                brightness: false,
            },
            expanded: Some(DetailOwner::Power),
            sink_count: 0,
            source_count: 0,
            profile_count: 0,
            bt_device_count: 0,
            monitor_count: 0,
            show_bluetooth: false,
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
        let rows = q.detail_rows(DetailOwner::Output);
        assert_eq!(rows.len(), 4, "3 sinks + Sound Settings");
        assert!(item(&rows[0]).selected && !item(&rows[1]).selected && !item(&rows[2]).selected);
        assert!(item(&rows[3]).separator_before && !item(&rows[3]).selected);
        assert_eq!(item(&rows[3]).label, "Sound Settings");
        // Row geometry agrees with the shape.
        assert_eq!(
            DetailOwner::Output.row_shape(q.sink_list.sinks.len()).len(),
            rows.len()
        );

        match q.pointer_click(center(detail_row_rect(1, q.layout()).unwrap())) {
            PopoverAction::SetOutputDevice(crate::audio::AudioDeviceKey::Node(name)) => {
                assert_eq!(name, "sink1")
            }
            other => panic!("expected SetOutputDevice, got {other:?}"),
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
        let rows = q.detail_rows(DetailOwner::Output);
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

    /// A rfkill snapshot with a usable soft Bluetooth switch (the tile's `available` gate).
    fn bt_rfkill_available() -> BluetoothRfkill {
        BluetoothRfkill {
            airplane: false,
            has_airplane: true,
            hardware_airplane: false,
        }
    }

    fn bt_device(alias: &str, connected: bool) -> crate::system_status::BluetoothDevice {
        crate::system_status::BluetoothDevice {
            path: format!("/org/bluez/hci0/dev_{alias}"),
            alias: alias.to_string(),
            icon: None,
            connectable: true,
            paired: true,
            trusted: false,
            connected,
        }
    }

    fn bt_status(
        powered: bool,
        devices: Vec<crate::system_status::BluetoothDevice>,
    ) -> BluetoothStatus {
        BluetoothStatus {
            adapter: Some("/org/bluez/hci0".to_string()),
            adapter_present: true,
            powered,
            state: if powered {
                BtAdapterState::On
            } else {
                BtAdapterState::Off
            },
            devices,
        }
    }

    /// A QS with the Bluetooth tile shown (rfkill available), a powered adapter, and `devices`.
    fn qs_bluetooth(
        powered: bool,
        devices: Vec<crate::system_status::BluetoothDevice>,
    ) -> QuickSettings {
        QuickSettings::new(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            PowerProfileStatus::default(),
            bt_status(powered, devices),
            bt_rfkill_available(),
            None,
            None,
            SinkList::default(),
            crate::audio::AudioCards::default(),
            false,
            MicStatus::default(),
            SourceList::default(),
            crate::brightness::BrightnessView::default(),
            [0, 0, 0],
        )
    }

    /// The Bluetooth tile is INSERTED at slot 1 (right after Network, GNOME's tile order,
    /// `panel.js:380-383`), gated on the rfkill `available`; with the appended conditionals also
    /// shown, PowerProfile shifts one right but still precedes Airplane.
    #[test]
    fn bluetooth_tile_inserts_at_slot_1_when_available() {
        // No soft switch → no tile (gnome-shell's `available`, bluetooth.js:103-108).
        let hidden = qs(NetworkStatus::Wired, None);
        assert!(!hidden.grid().contains(&GridTile::Bluetooth));

        let mut qs = qs_bluetooth(true, Vec::new());
        let tiles = qs.grid();
        assert_eq!(tiles.len(), 5);
        assert_eq!(tiles[0], GridTile::Network);
        assert_eq!(tiles[bluetooth_index()], GridTile::Bluetooth);
        assert_eq!(tiles[2], GridTile::Toggle(Tile::DarkStyle));

        // With power + airplane too: PowerProfile lands at the shifted index, Airplane last.
        qs.set_power_profile(PowerProfileStatus {
            active: "balanced".to_string(),
            available: vec![KnownProfile::Performance, KnownProfile::Balanced],
            show: true,
        });
        qs.set_airplane(AirplaneStatus {
            active: false,
            show: true,
        });
        let tiles = qs.grid();
        assert_eq!(tiles.len(), 7);
        assert_eq!(tiles[power_profile_index(true)], GridTile::PowerProfile);
        assert_eq!(tiles[6], GridTile::Airplane);

        // A hardware kill switch takes the tile away and collapses an open device list.
        qs.pointer_click(center(
            tile_arrow_rect(bluetooth_index(), GridTile::Bluetooth, qs.layout()).unwrap(),
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::Bluetooth));
        assert!(qs.set_bluetooth_rfkill(BluetoothRfkill {
            airplane: false,
            has_airplane: true,
            hardware_airplane: true,
        }));
        assert!(!qs.grid().contains(&GridTile::Bluetooth));
        assert!(
            qs.expanded.is_none(),
            "an open device list must collapse when the tile vanishes"
        );
    }

    /// The tile body click: checked = powered (`bluetooth.js:311-313`), the click returns the
    /// deferred toggle and optimistically shows the acquiring icon (`_predictedState`,
    /// `bluetooth.js:126-129`), which clears when a real adapter-state change echoes back
    /// (`bluetooth.js:53-56`) — or via the 30 s failsafe.
    #[test]
    fn bluetooth_body_toggles_with_a_predicted_acquiring_icon() {
        let mut qs = qs_bluetooth(true, Vec::new());
        let i = bluetooth_index();
        assert!(GridTile::Bluetooth.is_on(
            QuickToggles::default(),
            NetworkStatus::Wired,
            AirplaneStatus::default(),
            &PowerProfileStatus::default(),
            true,
        ));

        let action = qs.pointer_click(center(tile_body_rect(i, GridTile::Bluetooth, qs.layout())));
        assert!(matches!(action, PopoverAction::ToggleBluetooth));
        assert!(qs.bluetooth.powered, "echo-driven: no local flip");
        assert_eq!(qs.bt_effective_state(), BtAdapterState::TurningOff);
        assert_eq!(
            GridTile::Bluetooth.icons(
                NetworkStatus::Wired,
                &PowerProfileStatus::default(),
                qs.bt_effective_state()
            )[0],
            "bluetooth-acquiring-symbolic"
        );

        // A snapshot with an unchanged state does NOT clear the prediction…
        assert!(qs.set_bluetooth(bt_status(true, vec![bt_device("Buds", false)])));
        assert_eq!(qs.bt_effective_state(), BtAdapterState::TurningOff);
        // …but a real state change does.
        assert!(qs.set_bluetooth(bt_status(false, Vec::new())));
        assert_eq!(qs.bt_effective_state(), BtAdapterState::Off);

        // The failsafe path (nothing ever echoed).
        let mut qs2 = qs_bluetooth(false, Vec::new());
        qs2.pointer_click(center(tile_body_rect(i, GridTile::Bluetooth, qs2.layout())));
        assert_eq!(qs2.bt_effective_state(), BtAdapterState::TurningOn);
        assert!(qs2.clear_bluetooth_prediction());
        assert_eq!(qs2.bt_effective_state(), BtAdapterState::Off);
        assert!(!qs2.clear_bluetooth_prediction(), "idempotent");
    }

    /// The device list: the arrow opens `DetailOwner::Bluetooth` with rows sorted connected-first
    /// then alias, each with icon + alias + a Connect/Disconnect trailing label, then a separator
    /// and Bluetooth Settings; a row click returns the connect action and marks the row busy
    /// until `bluetooth_connect_done`.
    #[test]
    fn bluetooth_device_list_rows_and_connect_flow() {
        let mut qs = qs_bluetooth(
            true,
            vec![
                bt_device("Zeta", false),
                bt_device("Buds", true),
                bt_device("Alpha", false),
            ],
        );
        let i = bluetooth_index();
        let arrow = tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).expect("an arrow");
        assert!(matches!(
            qs.pointer_click(center(arrow)),
            PopoverAction::Consumed
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::Bluetooth));

        let rows = qs.detail_rows(DetailOwner::Bluetooth);
        assert_eq!(rows.len(), 4, "3 devices + Bluetooth Settings");
        assert_eq!(item(&rows[0]).label, "Buds");
        assert_eq!(item(&rows[0]).trailing.as_deref(), Some("Disconnect"));
        assert_eq!(item(&rows[1]).label, "Alpha");
        assert_eq!(item(&rows[1]).trailing.as_deref(), Some("Connect"));
        assert_eq!(item(&rows[2]).label, "Zeta");
        assert!(item(&rows[3]).separator_before);
        assert_eq!(item(&rows[3]).label, "Bluetooth Settings");

        // Clicking Alpha's row asks to connect and marks it busy…
        let alpha_path = "/org/bluez/hci0/dev_Alpha".to_string();
        match qs.pointer_click(center(detail_row_rect(1, qs.layout()).unwrap())) {
            PopoverAction::ConnectBluetoothDevice { path, connect } => {
                assert_eq!(path, alpha_path);
                assert!(connect);
            }
            other => panic!("expected ConnectBluetoothDevice, got {other:?}"),
        }
        let rows = qs.detail_rows(DetailOwner::Bluetooth);
        assert_eq!(item(&rows[1]).trailing.as_deref(), Some("…"), "busy mark");
        // …until the call reports done.
        assert!(qs.bluetooth_connect_done(&alpha_path));
        let rows = qs.detail_rows(DetailOwner::Bluetooth);
        assert_eq!(item(&rows[1]).trailing.as_deref(), Some("Connect"));

        // The settings row spawns control-center.
        match qs.pointer_click(center(detail_row_rect(3, qs.layout()).unwrap())) {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd, ["gnome-control-center", "bluetooth"]),
            other => panic!("expected a spawn, got {other:?}"),
        }
    }

    /// The order is frozen at open ("we don't reorder the list while the menu is open",
    /// `bluetooth.js:326-331,384-408`): a connection change mid-open must not re-sort; a new
    /// device appends; a removed device drops its row; reopening re-sorts.
    #[test]
    fn bluetooth_device_order_freezes_while_open() {
        let mut qs = qs_bluetooth(
            true,
            vec![bt_device("Buds", true), bt_device("Mouse", false)],
        );
        let i = bluetooth_index();
        let arrow = tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).unwrap();
        qs.pointer_click(center(arrow));

        // Mouse connects and Buds disconnects mid-open: sorted order would now lead with Mouse,
        // but the frozen order keeps Buds first.
        assert!(qs.set_bluetooth(bt_status(
            true,
            vec![bt_device("Buds", false), bt_device("Mouse", true)],
        )));
        let labels: Vec<_> = qs
            .detail_rows(DetailOwner::Bluetooth)
            .into_iter()
            .map(|r| item(&r).label.clone())
            .collect();
        assert_eq!(labels, ["Buds", "Mouse", "Bluetooth Settings"]);

        // A newcomer appends after the frozen rows (`addMenuItem`, bluetooth.js:398-408).
        assert!(qs.set_bluetooth(bt_status(
            true,
            vec![
                bt_device("Buds", false),
                bt_device("Mouse", true),
                bt_device("AAA", true),
            ],
        )));
        let labels: Vec<_> = qs
            .detail_rows(DetailOwner::Bluetooth)
            .into_iter()
            .map(|r| item(&r).label.clone())
            .collect();
        assert_eq!(labels, ["Buds", "Mouse", "AAA", "Bluetooth Settings"]);

        // Reopening re-sorts: connected first (AAA, Mouse), then alias (Buds).
        qs.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).unwrap(),
        )); // close
        qs.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).unwrap(),
        )); // reopen
        let labels: Vec<_> = qs
            .detail_rows(DetailOwner::Bluetooth)
            .into_iter()
            .map(|r| item(&r).label.clone())
            .collect();
        assert_eq!(labels, ["AAA", "Mouse", "Buds", "Bluetooth Settings"]);

        // Review F2: a powered flip while open rebuilds GNOME's list (`bluetooth.js:339-346`), so
        // the frozen order resets — power off (placeholder), power back on with a changed
        // connection mix → the rows come back freshly sorted, not in the pre-flip order.
        assert!(qs.set_bluetooth(bt_status(false, Vec::new())));
        assert!(item(&qs.detail_rows(DetailOwner::Bluetooth)[0]).placeholder);
        assert!(qs.set_bluetooth(bt_status(
            true,
            vec![
                bt_device("Buds", true),
                bt_device("Mouse", false),
                bt_device("AAA", false),
            ],
        )));
        let labels: Vec<_> = qs
            .detail_rows(DetailOwner::Bluetooth)
            .into_iter()
            .map(|r| item(&r).label.clone())
            .collect();
        assert_eq!(
            labels,
            ["Buds", "AAA", "Mouse", "Bluetooth Settings"],
            "a powered flip re-freezes to the fresh sort"
        );
    }

    /// The placeholder (`bluetooth.js:286-300,348-352`): no visible devices → one non-reactive
    /// centered line whose text depends on the adapter state, still followed by the settings row;
    /// it takes no hover and its click is consumed.
    #[test]
    fn bluetooth_placeholder_shows_without_devices() {
        // Powered, no devices.
        let mut qs = qs_bluetooth(true, Vec::new());
        let i = bluetooth_index();
        qs.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).unwrap(),
        ));
        let rows = qs.detail_rows(DetailOwner::Bluetooth);
        assert_eq!(rows.len(), 2);
        assert!(item(&rows[0]).placeholder);
        assert_eq!(item(&rows[0]).label, "No available or connected devices");
        assert_eq!(item(&rows[1]).label, "Bluetooth Settings");
        assert!(matches!(
            qs.pointer_click(center(detail_row_rect(0, qs.layout()).unwrap())),
            PopoverAction::Consumed
        ));
        assert!(
            !qs.pointer_hover(Some(center(detail_row_rect(0, qs.layout()).unwrap()))),
            "a placeholder row takes no hover highlight"
        );

        // Adapter off (devices are ignored while off, bluetooth.js:161-164).
        let mut qs = qs_bluetooth(false, vec![bt_device("Buds", true)]);
        qs.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).unwrap(),
        ));
        let rows = qs.detail_rows(DetailOwner::Bluetooth);
        assert!(item(&rows[0]).placeholder);
        assert_eq!(
            item(&rows[0]).label,
            "Turn on Bluetooth to connect to devices"
        );
    }

    /// The tile subtitle mirrors gnome-shell's `_sync` (`bluetooth.js:410-419`), and the detail
    /// card anchors below the Bluetooth tile's row (row 0, shared with Network).
    #[test]
    fn bluetooth_subtitle_and_anchor() {
        let mut qs = qs_bluetooth(true, vec![bt_device("Buds", true)]);
        assert_eq!(
            GridTile::Bluetooth
                .subtitle(&PowerProfileStatus::default(), &qs.bluetooth)
                .as_deref(),
            Some("Buds")
        );

        let i = bluetooth_index();
        qs.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, qs.layout()).unwrap(),
        ));
        let card = detail_rect(qs.layout()).expect("a card when expanded");
        let tile = tile_rect(i, qs.layout());
        assert!(
            (card.loc.y - (tile.loc.y + TILE_H + DETAIL_MARGIN)).abs() < 0.01,
            "card pins directly below the Bluetooth tile's row"
        );
    }

    /// Render differential for the new detail-card drawing: the open Bluetooth card bakes an
    /// opaque `%card` surface, and a device row (alias + trailing Connect/Disconnect sublabel)
    /// paints different ink than the centered placeholder. Self-skips without a Vulkan device.
    #[test]
    fn draws_the_bluetooth_detail_card() {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem, Texture as _};
        use smithay::utils::Buffer as BufferCoord;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_the_bluetooth_detail_card: no Vulkan device ({e})");
                return;
            }
        };

        let mut read_pixels = |qs: &QuickSettings| {
            let mut tex = qs.draw(&mut vk, 1.).expect("menu texture");
            let size = tex.size();
            let fb = vk.bind(&mut tex).expect("bind for readback");
            let region = Rectangle::<i32, BufferCoord>::from_size(size);
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .expect("copy_framebuffer");
            (
                vk.map_texture(&mapping).expect("map_texture").to_vec(),
                size,
            )
        };

        // One connected device, detail open.
        let mut with_device = qs_bluetooth(true, vec![bt_device("Buds", true)]);
        let i = bluetooth_index();
        with_device.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, with_device.layout()).unwrap(),
        ));
        let card = detail_rect(with_device.layout()).expect("card");
        let (pixels, size) = read_pixels(&with_device);

        // The card surface is opaque (the `%card` bg) just inside its top-left arc.
        let cx = (card.loc.x + DETAIL_RADIUS) as i32;
        let cy = (card.loc.y + DETAIL_RADIUS) as i32;
        let k = ((cy * size.w + cx) * 4) as usize;
        assert!(
            pixels[k + 3] > 200,
            "the detail card must bake an opaque surface, got alpha {}",
            pixels[k + 3]
        );

        // No devices → the placeholder variant; same geometry (one row), different row ink.
        let mut placeholder = qs_bluetooth(true, Vec::new());
        placeholder.pointer_click(center(
            tile_arrow_rect(i, GridTile::Bluetooth, placeholder.layout()).unwrap(),
        ));
        let (pixels2, size2) = read_pixels(&placeholder);
        assert_eq!(size, size2, "both cards have one row: same menu size");

        let row = detail_row_rect(0, with_device.layout()).expect("row 0");
        let mut differs = false;
        for y in (row.loc.y as i32)..((row.loc.y + row.size.h) as i32) {
            for x in (row.loc.x as i32)..((row.loc.x + row.size.w) as i32) {
                let k = ((y * size.w + x) * 4) as usize;
                if pixels[k..k + 4] != pixels2[k..k + 4] {
                    differs = true;
                    break;
                }
            }
        }
        assert!(
            differs,
            "a device row (alias + trailing sublabel) must paint different ink than the placeholder"
        );

        // Review F1: GNOME wraps the placeholder inside `2em 4em` padding
        // (`.bt-menu-placeholder`, `_quick-settings.scss:227-232`; `ellipsize: NONE` +
        // `line_wrap`, `bluetooth.js:291-294`); we draw ONE centered clipped line instead, which
        // is only an acceptable divergence while both strings actually FIT the row unclipped —
        // pin that, since `text_clipped` would truncate silently.
        {
            let row = detail_row_rect(0, placeholder.layout()).expect("placeholder row");
            let mut shaper = TextShaper::new(&mut vk, 1.);
            for text in [
                "Turn on Bluetooth to connect to devices",
                "No available or connected devices",
            ] {
                let run = shaper
                    .shape(text, TextStyle::new(DETAIL_PLACEHOLDER_PT).bold())
                    .expect("shape the placeholder");
                let ink_w = f64::from(run.ink_bounds().2);
                assert!(
                    ink_w < row.size.w,
                    "the placeholder must fit its row unclipped: {text:?} ink {ink_w} vs row {}",
                    row.size.w
                );
            }
        }
    }

    /// Review F5: with the Bluetooth tile inserted at slot 1, the Power Mode tile's index shifts
    /// — its picker card must still pin below the Power Mode tile's own (shifted) row, not the
    /// row the un-shifted index would name.
    #[test]
    fn power_picker_anchor_shifts_below_the_bluetooth_row() {
        let mut qs = qs_bluetooth(true, Vec::new());
        qs.set_power_profile(PowerProfileStatus {
            active: "performance".to_string(),
            available: vec![
                KnownProfile::Performance,
                KnownProfile::Balanced,
                KnownProfile::PowerSaver,
            ],
            show: true,
        });
        let ppi = power_profile_index(true);
        assert_eq!(qs.grid()[ppi], GridTile::PowerProfile);

        qs.pointer_click(center(
            tile_arrow_rect(ppi, GridTile::PowerProfile, qs.layout()).unwrap(),
        ));
        assert_eq!(qs.expanded, Some(DetailOwner::PowerProfile));
        let card = detail_rect(qs.layout()).expect("a card when expanded");
        let tile = tile_rect(ppi, qs.layout());
        assert!(
            (card.loc.y - (tile.loc.y + TILE_H + DETAIL_MARGIN)).abs() < 0.01,
            "card must pin below the SHIFTED Power Mode row (index {ppi}), got card y {} vs tile bottom {}",
            card.loc.y,
            tile.loc.y + TILE_H
        );
    }
}
