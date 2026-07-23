//! Fork-owned GNOME desktop policy.
//!
//! This module holds the inspectable model of the GNOME *settings* and policy the
//! compositor honors, kept deliberately separate from niri's own TOML config and
//! from the per-frame render path (see `docs/fork/STRATEGY.md`). GNOME policy
//! state flows through here as one inspectable struct rather than being scattered
//! across the input/render code.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gio::glib;
use gio::glib::prelude::ObjectExt;
use gio::prelude::{DBusProxyExt, SettingsExt, SettingsExtManual};
use smithay::input::keyboard::{xkb, Keysym};

use crate::world_clocks::{ResolvedLocation, WorldLocation};

/// The cached coordinate→timezone resolution: the parsed locations that produced
/// it, paired with the resolved output (see [`Stores::world_clocks_cache`]).
type WorldClocksCache = Option<(Vec<WorldLocation>, Vec<ResolvedLocation>)>;

/// GNOME desktop settings the compositor honors, mirroring the relevant
/// `org.gnome.*` GSettings keys.
///
/// [`Default`] is GNOME's compiled-in defaults; [`load_and_watch_gsettings`]
/// overlays the live values read from the user's GSettings/dconf backend — the
/// same store gnome-shell/mutter use — and keeps the model current afterwards.
/// Detection code reads through this model, so updates never need to touch the
/// input path.
#[derive(Debug, Clone)]
pub struct GnomeSettings {
    /// `org.gnome.mutter overlay-key`: the keys whose lone tap toggles the
    /// Activities overview. Empty disables the overlay key. GNOME's default is
    /// `"Super"`, which means *either* Super (`Super_L` and `Super_R`).
    pub overlay_keys: Vec<Keysym>,
    /// The GNOME keybindings we honor, from `org.gnome.desktop.wm.keybindings`
    /// (the schema shared by mutter and gnome-shell; see
    /// [`adopted_wm_keybindings`] for the subset). One entry per adopted
    /// settings key, in table order — the input path returns the first match.
    pub keybindings: Vec<GnomeKeybinding>,
    /// `org.gnome.shell command-history`: the run dialog's persisted history,
    /// oldest first. gnome-shell caps it at 512 entries.
    pub command_history: Vec<String>,
    /// `org.gnome.shell favorite-apps`: the dash's pinned apps, in order
    /// (`js/ui/appFavorites.js`). Raw stored ids; the [`AppSystem`] resolves them.
    ///
    /// [`AppSystem`]: crate::app_system::AppSystem
    pub favorite_apps: Vec<String>,
    /// `org.gnome.desktop.lockdown disable-command-line`: when set, the run
    /// dialog refuses to open (gnome-shell's `RunDialog.open`).
    pub disable_command_line: bool,
    /// `org.gnome.desktop.wm.preferences focus-new-windows`: whether new
    /// windows may take focus on map.
    pub focus_new_windows: FocusNewWindows,
    /// `org.gnome.mutter edge-tiling`: whether dragging a window to a screen
    /// edge tiles (sides) or maximizes (top) it.
    pub edge_tiling: bool,
    /// `org.gnome.desktop.background`: the wallpaper GNOME would draw.
    pub background: BackgroundSettings,
    /// `org.gnome.desktop.interface accent-color`, resolved to RGB with
    /// gnome-shell's palette (st-theme-context.c). Drives accent-colored
    /// chrome like the overview thumbnail indicator.
    pub accent_color: [u8; 3],
    /// `org.gnome.desktop.interface icon-theme`: the icon theme both the symbolic
    /// icon cache and the app-icon loader resolve against. GNOME's default is
    /// `"Adwaita"`.
    pub icon_theme: String,
    /// `org.gnome.desktop.interface clock-*`: how the panel clock label reads.
    pub clock: ClockFormat,
    /// `org.gnome.desktop.calendar`: week start + week-number column.
    pub calendar: CalendarSettings,
    /// The state of the quick-settings toggles we back with gsettings.
    pub quick_toggles: QuickToggles,
    /// `org.gnome.shell last-selected-power-profile`: the non-Balanced profile the Power Mode
    /// tile's body-click toggles back to (gnome-shell's `PowerProfilesToggle`). GNOME's schema
    /// default is `"power-saver"`.
    pub last_power_profile: String,
    /// `org.gnome.desktop.input-sources`: the configured keyboard layouts (GNOME's
    /// way replaces niri's `input.keyboard.xkb` — see the CLAUDE.md tenet). Drives
    /// the seat keymap and the panel input-source indicator.
    pub input_sources: InputSources,
    /// `org.gnome.shell.world-clocks locations` resolved to timezones, plus whether
    /// GNOME Clocks is installed — drives the dateMenu World Clocks section.
    pub world_clocks: WorldClocks,
}

/// The dateMenu World Clocks section's data (`js/ui/dateMenu.js` `WorldClocksSection`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorldClocks {
    /// Whether `org.gnome.clocks.desktop` is installed — the whole section shows
    /// iff true (`_sync`, `dateMenu.js:384-387`). Sampled once at startup (a
    /// divergence from GNOME's live `installed-changed`; needs a relog to update).
    pub clocks_installed: bool,
    /// The configured clocks with their coordinate-resolved IANA timezones, in
    /// settings order (the UI sorts by current offset at render time).
    pub locations: Vec<crate::world_clocks::ResolvedLocation>,
}

/// The keyboard input-source configuration from `org.gnome.desktop.input-sources`
/// (`js/ui/status/keyboard.js` `InputSourceSessionSettings`). We read the layout
/// list, MRU ordering, options and model; the deprecated `current` key is ignored
/// (GNOME 50.1 tracks the active source via `mru-sources`, not `current`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputSources {
    /// Whether the schema is installed. When true its values — even empty ones —
    /// are the source of truth (GNOME wins); when false we fall back to niri's
    /// xkb config, then systemd-localed.
    pub present: bool,
    /// `sources` `a(ss)`: `(type, id)` in order. `type` is `"xkb"` or `"ibus"`;
    /// an xkb `id` is `"layout"` or `"layout+variant"`.
    pub sources: Vec<(String, String)>,
    /// `mru-sources` `a(ss)`: most-recently-used ordering, same tuple format —
    /// what GNOME writes on an interactive switch (the active source is
    /// `mru-sources[0]`).
    pub mru_sources: Vec<(String, String)>,
    /// `xkb-options` `as`.
    pub xkb_options: Vec<String>,
    /// `xkb-model` `s`.
    pub xkb_model: String,
}

/// The gsettings-backed quick-settings toggles (the "self-contained" ones that
/// need no daemon): each mirrors one key, read for the tile's on/off state and
/// written back by [`GnomeSettingsWriter`] when the tile is clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuickToggles {
    /// `org.gnome.desktop.interface color-scheme == "prefer-dark"` (Dark Style).
    pub dark_style: bool,
    /// Do Not Disturb: the *inverse* of `org.gnome.desktop.notifications
    /// show-banners` (gnome-shell's DND tile hides banners).
    pub do_not_disturb: bool,
    /// `org.gnome.settings-daemon.plugins.color night-light-enabled`.
    pub night_light: bool,
}

impl Default for GnomeSettings {
    fn default() -> Self {
        Self {
            overlay_keys: vec![Keysym::Super_L, Keysym::Super_R],
            keybindings: default_keybindings(),
            command_history: Vec::new(),
            favorite_apps: Vec::new(),
            disable_command_line: false,
            focus_new_windows: FocusNewWindows::Smart,
            edge_tiling: true,
            background: BackgroundSettings::default(),
            accent_color: ACCENT_BLUE,
            icon_theme: "Adwaita".to_string(),
            clock: ClockFormat::default(),
            calendar: CalendarSettings::default(),
            quick_toggles: QuickToggles::default(),
            last_power_profile: "power-saver".to_string(),
            input_sources: InputSources::default(),
            world_clocks: WorldClocks::default(),
        }
    }
}

/// GNOME's default accent (st-theme-context.c `ACCENT_COLOR_BLUE`).
pub const ACCENT_BLUE: [u8; 3] = [0x35, 0x84, 0xe4];

/// The wallpaper settings from `org.gnome.desktop.background`, already
/// resolved down to what the compositor needs to draw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackgroundSettings {
    /// The picture to draw: `picture-uri` or `picture-uri-dark` (selected by
    /// `org.gnome.desktop.interface color-scheme`, like gnome-shell's
    /// `Background._loadBackground`), converted to a local path. `None` when
    /// the URI is empty/non-local or `picture-options` is `none`.
    pub picture: Option<PathBuf>,
    /// `picture-options`: how the picture is fit to the screen.
    pub options: BackgroundOptions,
}

/// `org.gnome.desktop.background picture-options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundOptions {
    /// Don't draw the picture at all.
    None,
    /// Tile at native size.
    Wallpaper,
    /// Center at native size.
    Centered,
    /// Fit inside the screen, keeping aspect (letterboxed).
    Scaled,
    /// Stretch to the screen, ignoring aspect.
    Stretched,
    /// Cover the screen, keeping aspect (center-cropped). The GNOME default.
    #[default]
    Zoom,
    /// One picture spanned across all monitors.
    Spanned,
}

/// The panel clock label format from `org.gnome.desktop.interface`, the same
/// keys gnome-shell's `GnomeDesktop.WallClock` formats from (`dateMenu.js`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockFormat {
    /// `clock-format`: 24-hour when true, else 12-hour with an AM/PM suffix.
    pub hour24: bool,
    /// `clock-show-weekday`: prefix the abbreviated weekday.
    pub show_weekday: bool,
    /// `clock-show-date`: include the abbreviated month and day-of-month.
    pub show_date: bool,
    /// `clock-show-seconds`: include seconds (and tick the clock every second).
    pub show_seconds: bool,
}

impl Default for ClockFormat {
    fn default() -> Self {
        // A bare `HH:MM` fallback for when the interface schema is absent; the
        // live gsettings values override it on a real GNOME session.
        Self {
            hour24: true,
            show_weekday: false,
            show_date: false,
            show_seconds: false,
        }
    }
}

/// The calendar popover settings from `org.gnome.desktop.calendar`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalendarSettings {
    /// First day of the week, 0=Sunday..6=Saturday (`week-start-day`, with
    /// `'default'` resolved to the locale like `Shell.util_get_week_start`).
    pub week_start: u8,
    /// `show-weekdate`: show the ISO week-number column.
    pub show_week_numbers: bool,
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self {
            week_start: locale_week_start(),
            show_week_numbers: false,
        }
    }
}

/// The locale's first day of the week, 0=Sunday..6=Saturday. Faithful port of
/// gnome-shell's `shell_util_get_week_start` (itself copied from `gtkcalendar.c`):
/// combine `_NL_TIME_FIRST_WEEKDAY` (a byte, 1=Sunday..7=Saturday, giving the
/// offset from the week-origin date) with `_NL_TIME_WEEK_1STDAY` (a *packed date*
/// read as the pointer's integer value, not a string — 19971130=Sunday origin,
/// 19971201=Monday origin), as `(week_1stday + first_weekday - 1) % 7`.
fn locale_week_start() -> u8 {
    // glibc `_NL_ITEM(LC_TIME=2, index)` = `(2 << 16) | index`. The correct indices are
    // 0x68 / 0x66 — NOT 14, which is `ABMON_1` ("Jan"), whose 'J' byte silently yielded
    // Wednesday. Verified against the system langinfo.h.
    const _NL_TIME_FIRST_WEEKDAY: libc::nl_item = 0x20068;
    const _NL_TIME_WEEK_1STDAY: libc::nl_item = 0x20066;
    // SAFETY: nl_langinfo returns a pointer into static locale data. FIRST_WEEKDAY points at a
    // string whose first byte is the weekday; WEEK_1STDAY is the glibc quirk where the *pointer
    // value itself* is the packed date integer (per gtkcalendar.c's `union { uint; char*; }`).
    unsafe {
        let fw = libc::nl_langinfo(_NL_TIME_FIRST_WEEKDAY);
        let first_weekday = if fw.is_null() { 1i32 } else { *fw as i32 };
        let week_origin = libc::nl_langinfo(_NL_TIME_WEEK_1STDAY) as usize as u32;
        let week_1stday = match week_origin {
            19971130 => 0, // Sunday origin
            19971201 => 1, // Monday origin
            _ => 0,        // unknown → assume Sunday (GNOME warns; we default quietly)
        };
        (((week_1stday + first_weekday - 1) % 7 + 7) % 7) as u8
    }
}

/// `org.gnome.desktop.wm.preferences focus-new-windows`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusNewWindows {
    /// New windows take focus unless an intervening user interaction with the
    /// focused window makes that a focus steal. The GNOME default.
    Smart,
    /// New windows never take focus unless transient for the focused window.
    Strict,
}

impl GnomeSettings {
    fn load_mutter(&mut self, mutter: &gio::Settings) {
        let overlay_key = mutter.string("overlay-key");
        match parse_overlay_key(overlay_key.as_str()) {
            Ok(keys) => self.overlay_keys = keys,
            Err(name) => warn!("ignoring unrecognized org.gnome.mutter overlay-key {name:?}"),
        }
        if settings_has_key(mutter, "edge-tiling") {
            self.edge_tiling = mutter.boolean("edge-tiling");
        }
    }

    fn load_shell(&mut self, shell: &gio::Settings) {
        if settings_has_key(shell, "command-history") {
            self.command_history = shell
                .strv("command-history")
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
        if settings_has_key(shell, "last-selected-power-profile") {
            self.last_power_profile = shell.string("last-selected-power-profile").to_string();
        }
        if settings_has_key(shell, "favorite-apps") {
            self.favorite_apps = shell
                .strv("favorite-apps")
                .iter()
                .map(|s| s.to_string())
                .collect();
        }
    }

    fn load_lockdown(&mut self, lockdown: &gio::Settings) {
        if settings_has_key(lockdown, "disable-command-line") {
            self.disable_command_line = lockdown.boolean("disable-command-line");
        }
    }

    fn load_background(&mut self, background: &gio::Settings, interface: Option<&gio::Settings>) {
        // gnome-shell picks the dark variant whenever color-scheme is
        // prefer-dark (js/ui/background.js, `_loadBackground`).
        let prefer_dark = interface
            .filter(|s| settings_has_key(s, "color-scheme"))
            .is_some_and(|s| s.string("color-scheme") == "prefer-dark");

        let uri_key = if prefer_dark && settings_has_key(background, "picture-uri-dark") {
            "picture-uri-dark"
        } else {
            "picture-uri"
        };
        if !settings_has_key(background, uri_key) {
            return;
        }
        let uri = background.string(uri_key);

        let options = if settings_has_key(background, "picture-options") {
            parse_picture_options(background.string("picture-options").as_str())
        } else {
            BackgroundOptions::default()
        };

        self.background = BackgroundSettings {
            picture: resolve_picture_uri(uri.as_str(), options),
            options,
        };
    }

    fn load_interface(&mut self, interface: &gio::Settings) {
        if settings_has_key(interface, "accent-color") {
            let value = interface.string("accent-color");
            match parse_accent_color(value.as_str()) {
                Some(rgb) => self.accent_color = rgb,
                None => warn!("ignoring unrecognized accent-color {value:?}"),
            }
        }
        if settings_has_key(interface, "icon-theme") {
            let value = interface.string("icon-theme");
            if !value.is_empty() {
                self.icon_theme = value.to_string();
            }
        }
        if settings_has_key(interface, "clock-format") {
            // The key is the enum "12h"/"24h"; treat anything else as 24-hour.
            self.clock.hour24 = interface.string("clock-format").as_str() != "12h";
        }
        if settings_has_key(interface, "clock-show-weekday") {
            self.clock.show_weekday = interface.boolean("clock-show-weekday");
        }
        if settings_has_key(interface, "clock-show-date") {
            self.clock.show_date = interface.boolean("clock-show-date");
        }
        if settings_has_key(interface, "clock-show-seconds") {
            self.clock.show_seconds = interface.boolean("clock-show-seconds");
        }
        if settings_has_key(interface, "color-scheme") {
            self.quick_toggles.dark_style = interface.string("color-scheme") == "prefer-dark";
        }
    }

    fn load_input_sources(&mut self, s: &gio::Settings) {
        // The schema being present makes it the source of truth (GNOME wins),
        // even if the arrays are empty (→ the "us" fallback downstream).
        self.input_sources.present = true;
        if settings_has_key(s, "sources") {
            self.input_sources.sources = read_source_tuples(&s.value("sources"));
        }
        if settings_has_key(s, "mru-sources") {
            self.input_sources.mru_sources = read_source_tuples(&s.value("mru-sources"));
        }
        if settings_has_key(s, "xkb-options") {
            self.input_sources.xkb_options = s
                .strv("xkb-options")
                .iter()
                .map(|o| o.to_string())
                .collect();
        }
        if settings_has_key(s, "xkb-model") {
            self.input_sources.xkb_model = s.string("xkb-model").to_string();
        }
    }

    /// Read `org.gnome.shell.world-clocks locations` and resolve each location's
    /// coordinates to a timezone (`WorldClocksSection._clocksChanged`,
    /// `dateMenu.js:389-408`). The `tzf-rs` finder is expensive, so its output is
    /// cached against the parsed locations: only a genuine change to the blob pays
    /// the resolution cost, not every unrelated settings re-read.
    fn load_world_clocks(&mut self, s: &gio::Settings, cache: &RefCell<WorldClocksCache>) {
        if !settings_has_key(s, "locations") {
            return;
        }
        let locations = crate::world_clocks::parse_locations(&s.value("locations"));
        let mut cache = cache.borrow_mut();
        let resolved = match &*cache {
            Some((cached_locs, cached_resolved)) if *cached_locs == locations => {
                cached_resolved.clone()
            }
            _ => {
                let resolved = crate::world_clocks::resolve_timezones(&locations);
                *cache = Some((locations, resolved.clone()));
                resolved
            }
        };
        self.world_clocks.locations = resolved;
    }

    fn load_notifications(&mut self, notifications: &gio::Settings) {
        // gnome-shell's Do Not Disturb tile is the inverse of show-banners
        // (js/ui/status/system.js / calendar.js `_setDndState`).
        if settings_has_key(notifications, "show-banners") {
            self.quick_toggles.do_not_disturb = !notifications.boolean("show-banners");
        }
    }

    fn load_color(&mut self, color: &gio::Settings) {
        if settings_has_key(color, "night-light-enabled") {
            self.quick_toggles.night_light = color.boolean("night-light-enabled");
        }
    }

    fn load_calendar(&mut self, calendar: &gio::Settings) {
        if settings_has_key(calendar, "show-weekdate") {
            self.calendar.show_week_numbers = calendar.boolean("show-weekdate");
        }
        if settings_has_key(calendar, "week-start-day") {
            let value = calendar.string("week-start-day");
            self.calendar.week_start = match value.as_str() {
                "sunday" => 0,
                "monday" => 1,
                "tuesday" => 2,
                "wednesday" => 3,
                "thursday" => 4,
                "friday" => 5,
                "saturday" => 6,
                "default" => locale_week_start(),
                other => {
                    warn!("ignoring unrecognized week-start-day {other:?}");
                    locale_week_start()
                }
            };
        }
    }

    fn load_wm_preferences(&mut self, wm: &gio::Settings) {
        if settings_has_key(wm, "focus-new-windows") {
            let value = wm.string("focus-new-windows");
            match value.as_str() {
                "smart" => self.focus_new_windows = FocusNewWindows::Smart,
                "strict" => self.focus_new_windows = FocusNewWindows::Strict,
                other => warn!("ignoring unrecognized focus-new-windows {other:?}"),
            }
        }
    }

    /// (Re-)builds the keybinding list from both adopted tables, reading each
    /// from its store where open and falling back to our built-in defaults.
    fn load_keybindings(
        &mut self,
        wm: Option<&gio::Settings>,
        mutter_keybindings: Option<&gio::Settings>,
    ) {
        let mut keybindings = read_keybinding_table(wm, adopted_wm_keybindings());
        keybindings.extend(read_keybinding_table(
            mutter_keybindings,
            adopted_mutter_keybindings(),
        ));
        self.keybindings = keybindings;
    }
}

/// Reads one adopted-keybindings table, one entry per settings key in table
/// order, from its store where open (a missing key falls back to our
/// built-in default rather than aborting inside gio).
fn read_keybinding_table(
    store: Option<&gio::Settings>,
    table: Vec<(String, GnomeKeyAction, Vec<String>)>,
) -> Vec<GnomeKeybinding> {
    table
        .into_iter()
        .map(|(key, action, defaults)| {
            let values = match store {
                Some(store) if settings_has_key(store, &key) => {
                    store.strv(&key).iter().map(|s| s.to_string()).collect()
                }
                Some(_) => {
                    warn!("keybindings schema has no {key:?}; using our default");
                    defaults
                }
                None => defaults,
            };
            GnomeKeybinding {
                action,
                accels: parse_accels(&key, values),
            }
        })
        .collect()
}

fn settings_has_key(settings: &gio::Settings, key: &str) -> bool {
    settings
        .settings_schema()
        .is_some_and(|schema| schema.has_key(key))
}

/// Unpack an `a(ss)` variant (the `sources` / `mru-sources` keys) into `(type, id)`
/// pairs. Children that don't decode as `(String, String)` are skipped.
fn read_source_tuples(value: &glib::Variant) -> Vec<(String, String)> {
    (0..value.n_children())
        .filter_map(|i| value.child_value(i).get::<(String, String)>())
        .collect()
}

/// One GNOME keybinding we honor: a semantic action and the accelerators
/// currently bound to it (possibly none — an unbound action).
#[derive(Debug, Clone, PartialEq)]
pub struct GnomeKeybinding {
    pub action: GnomeKeyAction,
    pub accels: Vec<Accel>,
}

/// The semantic actions of the adopted GNOME keybindings, named after their
/// `org.gnome.desktop.wm.keybindings` keys. The four directional workspace
/// keys collapse to previous/next: GNOME's workspaces are one linear row
/// (left/right; up/down are the pre-GNOME-40 vertical-layout legacy), and both
/// axes map onto niri's vertical workspace column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GnomeKeyAction {
    /// `panel-run-dialog`: open the Alt+F2 run dialog.
    PanelRunDialog,
    /// `close`: close the focused window.
    Close,
    /// `toggle-fullscreen`: fullscreen the focused window.
    ToggleFullscreen,
    /// `switch-to-workspace-N` (1-based, like the settings keys).
    SwitchToWorkspace(u8),
    /// `switch-to-workspace-left` and `-up`.
    SwitchToWorkspacePrevious,
    /// `switch-to-workspace-right` and `-down`.
    SwitchToWorkspaceNext,
    /// `move-to-workspace-N` (1-based).
    MoveToWorkspace(u8),
    /// `move-to-workspace-left` and `-up`.
    MoveToWorkspacePrevious,
    /// `move-to-workspace-right` and `-down`.
    MoveToWorkspaceNext,
    /// `switch-windows` / `switch-windows-backward`: cycle windows of the
    /// current workspace (GNOME's window switcher is per-workspace by
    /// default).
    SwitchWindows { backward: bool },
    /// `switch-applications` / `switch-applications-backward`: GNOME's
    /// Alt-Tab. GNOME groups by application and spans workspaces; we map it
    /// onto the window MRU switcher over all workspaces (no app grouping —
    /// accepted divergence for now).
    SwitchApplications { backward: bool },
    /// `maximize`: maximize the focused window.
    Maximize,
    /// `unmaximize`: unmaximize the focused window; a tiled window untiles.
    Unmaximize,
    /// `toggle-tiled-left` / `-right` (`org.gnome.mutter.keybindings`): tile
    /// the focused window to the given half of the work area, or untile it if
    /// already tiled there.
    ToggleTiled(TileSide),
}

/// Which half of the work area a window is tiled to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileSide {
    Left,
    Right,
}

/// What dropping a dragged window on a screen edge does (mutter
/// `meta-window-drag.c`, `update_move_maybe_tile`): the side bands tile, the
/// band above the work area maximizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeTileTarget {
    Tile(TileSide),
    Maximize,
}

/// mutter's "shake threshold" (`meta-window-drag.c`): the default mouse
/// drag-threshold (8) × DRAG_THRESHOLD_TO_SHAKE_THRESHOLD_FACTOR (6). Both the
/// width of the edge-tile drop zones and how far a drag must travel vertically
/// to shake a maximized window loose.
pub const SHAKE_THRESHOLD: f64 = 48.;

/// The `org.gnome.desktop.wm.keybindings` keys we honor, with GNOME's default
/// accelerators for each. The defaults are the GNOME session's effective ones
/// (gnome-shell overrides some upstream schema defaults, e.g. `<Super>N` for
/// workspace switching); they only matter where the schema isn't installed,
/// since a live read replaces them wholesale.
fn adopted_wm_keybindings() -> Vec<(String, GnomeKeyAction, Vec<String>)> {
    use GnomeKeyAction::*;

    fn strs(defaults: &[&str]) -> Vec<String> {
        defaults.iter().map(|s| (*s).to_owned()).collect()
    }

    let mut keys = vec![
        (
            "panel-run-dialog".to_owned(),
            PanelRunDialog,
            strs(&["<Alt>F2"]),
        ),
        ("close".to_owned(), Close, strs(&["<Alt>F4"])),
        ("toggle-fullscreen".to_owned(), ToggleFullscreen, strs(&[])),
        ("maximize".to_owned(), Maximize, strs(&["<Super>Up"])),
        (
            "unmaximize".to_owned(),
            Unmaximize,
            strs(&["<Super>Down", "<Alt>F5"]),
        ),
        (
            "switch-to-workspace-left".to_owned(),
            SwitchToWorkspacePrevious,
            strs(&[
                "<Super>Page_Up",
                "<Super>KP_Prior",
                "<Super><Alt>Left",
                "<Control><Alt>Left",
            ]),
        ),
        (
            "switch-to-workspace-right".to_owned(),
            SwitchToWorkspaceNext,
            strs(&[
                "<Super>Page_Down",
                "<Super>KP_Next",
                "<Super><Alt>Right",
                "<Control><Alt>Right",
            ]),
        ),
        (
            "switch-to-workspace-up".to_owned(),
            SwitchToWorkspacePrevious,
            strs(&["<Control><Alt>Up"]),
        ),
        (
            "switch-to-workspace-down".to_owned(),
            SwitchToWorkspaceNext,
            strs(&["<Control><Alt>Down"]),
        ),
        (
            "move-to-workspace-left".to_owned(),
            MoveToWorkspacePrevious,
            strs(&[
                "<Super><Shift>Page_Up",
                "<Super><Shift>KP_Prior",
                "<Super><Shift><Alt>Left",
                "<Control><Shift><Alt>Left",
            ]),
        ),
        (
            "move-to-workspace-right".to_owned(),
            MoveToWorkspaceNext,
            strs(&[
                "<Super><Shift>Page_Down",
                "<Super><Shift>KP_Next",
                "<Super><Shift><Alt>Right",
                "<Control><Shift><Alt>Right",
            ]),
        ),
        (
            "move-to-workspace-up".to_owned(),
            MoveToWorkspacePrevious,
            strs(&["<Control><Shift><Alt>Up"]),
        ),
        (
            "move-to-workspace-down".to_owned(),
            MoveToWorkspaceNext,
            strs(&["<Control><Shift><Alt>Down"]),
        ),
        (
            "switch-windows".to_owned(),
            SwitchWindows { backward: false },
            strs(&["<Alt>Tab"]),
        ),
        (
            "switch-windows-backward".to_owned(),
            SwitchWindows { backward: true },
            strs(&["<Alt><Shift>Tab"]),
        ),
        (
            "switch-applications".to_owned(),
            SwitchApplications { backward: false },
            strs(&["<Super>Tab"]),
        ),
        (
            "switch-applications-backward".to_owned(),
            SwitchApplications { backward: true },
            strs(&["<Shift><Super>Tab"]),
        ),
    ];

    for n in 1..=12u8 {
        let defaults = if n <= 4 {
            vec![format!("<Super>{n}")]
        } else {
            Vec::new()
        };
        keys.push((
            format!("switch-to-workspace-{n}"),
            SwitchToWorkspace(n),
            defaults,
        ));

        let defaults = if n == 1 {
            strs(&["<Super><Shift>Home"])
        } else {
            Vec::new()
        };
        keys.push((
            format!("move-to-workspace-{n}"),
            MoveToWorkspace(n),
            defaults,
        ));
    }

    keys
}

fn default_keybindings() -> Vec<GnomeKeybinding> {
    let mut keybindings = read_keybinding_table(None, adopted_wm_keybindings());
    keybindings.extend(read_keybinding_table(None, adopted_mutter_keybindings()));
    keybindings
}

/// Parse a settings key's accelerator array, mirroring mutter's
/// `update_binding`: an invalid entry is warned about and skipped without
/// poisoning the valid rest, and disabled entries simply don't bind.
fn parse_accels(key: &str, values: impl IntoIterator<Item = String>) -> Vec<Accel> {
    let mut accels = Vec::new();
    for value in values {
        match parse_accelerator(&value) {
            Ok(Some(accel)) => accels.push(accel),
            Ok(None) => {}
            Err(()) => warn!("ignoring unparseable accelerator {value:?} for {key:?}"),
        }
    }
    accels
}

/// Read the current [`GnomeSettings`] and watch the GSettings store, delivering
/// a freshly-read model over the returned channel whenever a setting we honor
/// changes.
///
/// GSettings change notification needs a glib main loop, and the compositor
/// runs calloop — so a dedicated thread runs a private glib [`MainContext`] and
/// forwards each re-read model over a calloop channel for the main loop to
/// apply. The *initial* read also happens on that thread (handed back through a
/// handshake): the GSettings backend singleton binds its change notification to
/// the main context that is thread-default on the process's first GSettings
/// use, so every touch of the store must come from the thread whose loop
/// actually runs. On non-GNOME systems (schema not installed) the initial model
/// is the defaults and the channel stays silent.
///
/// [`MainContext`]: glib::MainContext
pub fn load_and_watch_gsettings() -> (
    GnomeSettings,
    calloop::channel::Channel<GnomeSettings>,
    GnomeSettingsWriter,
) {
    let (tx, rx) = calloop::channel::channel();
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    let ctx = glib::MainContext::new();
    let writer = GnomeSettingsWriter { ctx: ctx.clone() };

    std::thread::Builder::new()
        .name("gsettings-watch".to_owned())
        .spawn(move || {
            ctx.with_thread_default(|| {
                let stores = Rc::new(Stores::open());
                STORES.set(Some(stores.clone()));

                // Subscribe before the initial read so no change can fall
                // between them; a racing change just re-arrives via `tx`.
                stores.subscribe(move |settings| {
                    let _ = tx.send(settings);
                });
                // Mirror GNOME Clocks' locations into the shell gsettings before the
                // initial read, so a running Clocks is reflected in the first model.
                stores.setup_clocks_mirror();
                let _ = init_tx.send(stores.read());

                if stores.any() {
                    glib::MainLoop::new(Some(&ctx), false).run();
                }
            })
            .unwrap();
        })
        .unwrap();

    let initial = init_rx.recv().unwrap_or_else(|_| {
        warn!("GSettings watcher thread died during startup; using GNOME defaults");
        GnomeSettings::default()
    });
    (initial, rx, writer)
}

thread_local! {
    /// The watcher thread's stores, for [`GnomeSettingsWriter`] closures. The
    /// stores are not `Send`, so a write request can't carry them — it finds
    /// them here after hopping onto the watcher thread.
    static STORES: std::cell::Cell<Option<Rc<Stores>>> = const { std::cell::Cell::new(None) };
}

/// Writes settings back to the GSettings store, from any thread.
///
/// Like every touch of the store, writes must happen on the watcher thread
/// (see [`load_and_watch_gsettings`]); this hops there via the glib main
/// context. A successful write comes back around through the change
/// subscription, updating the model like any external change.
#[derive(Clone)]
pub struct GnomeSettingsWriter {
    ctx: glib::MainContext,
}

impl GnomeSettingsWriter {
    /// Persist the run dialog's command history to `org.gnome.shell
    /// command-history`.
    pub fn set_command_history(&self, history: Vec<String>) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(shell) = &s.shell {
                    if settings_has_key(shell, "command-history") {
                        let history: Vec<&str> = history.iter().map(String::as_str).collect();
                        if let Err(err) = shell.set_strv("command-history", history) {
                            warn!("error writing org.gnome.shell command-history: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Persist the dash's pinned apps to `org.gnome.shell favorite-apps`
    /// (`js/ui/appFavorites.js` `_updateFavorites`). Missing store/key is a no-op,
    /// so the authoritative copy lives in the `AppSystem` and this is best-effort
    /// persistence (like [`set_command_history`](Self::set_command_history)).
    pub fn set_favorite_apps(&self, favorites: Vec<String>) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(shell) = &s.shell {
                    if settings_has_key(shell, "favorite-apps") {
                        let favorites: Vec<&str> = favorites.iter().map(String::as_str).collect();
                        if let Err(err) = shell.set_strv("favorite-apps", favorites) {
                            warn!("error writing org.gnome.shell favorite-apps: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Persist the last non-Balanced power profile to `org.gnome.shell
    /// last-selected-power-profile` (gnome-shell's `PowerProfilesToggle._sync`). Takes a runtime
    /// `String` (unlike [`set_string`](Self::set_string)'s `&'static str`). Missing store/key is a
    /// no-op, so the authoritative copy lives on `Niri` and this is best-effort persistence.
    pub fn set_last_power_profile(&self, profile: String) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(shell) = &s.shell {
                    if settings_has_key(shell, "last-selected-power-profile") {
                        if let Err(err) = shell.set_string("last-selected-power-profile", &profile)
                        {
                            warn!(
                                "error writing org.gnome.shell last-selected-power-profile: {err}"
                            );
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Record the most-recently-used input sources: `org.gnome.desktop.input-sources
    /// mru-sources` (`a(ss)`). gnome-shell writes only this on an interactive
    /// layout switch (the deprecated `current` key is left untouched). Missing
    /// store/key is a no-op.
    pub fn set_mru_sources(&self, sources: Vec<(String, String)>) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(input_sources) = &s.input_sources {
                    if settings_has_key(input_sources, "mru-sources") {
                        let variant = glib::prelude::ToVariant::to_variant(&sources);
                        if let Err(err) = input_sources.set_value("mru-sources", &variant) {
                            warn!(
                                "error writing org.gnome.desktop.input-sources mru-sources: {err}"
                            );
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Dark Style tile: `org.gnome.desktop.interface color-scheme`
    /// (`prefer-dark` on, `default` off — matching gnome-shell's tile).
    pub fn set_dark_style(&self, dark: bool) {
        let value = if dark { "prefer-dark" } else { "default" };
        self.set_string("interface", "color-scheme", value);
    }

    /// Do Not Disturb tile: the *inverse* of `org.gnome.desktop.notifications
    /// show-banners`.
    pub fn set_do_not_disturb(&self, dnd: bool) {
        self.set_bool("notifications", "show-banners", !dnd);
    }

    /// Night Light tile: `org.gnome.settings-daemon.plugins.color
    /// night-light-enabled`.
    pub fn set_night_light(&self, on: bool) {
        self.set_bool("color", "night-light-enabled", on);
    }

    /// Write a string key on one of the stores (named by [`Stores::get`]),
    /// hopping onto the watcher thread first. Missing store/key is a no-op.
    fn set_string(&self, store: &'static str, key: &'static str, value: &'static str) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(settings) = s.get(store) {
                    if settings_has_key(settings, key) {
                        if let Err(err) = settings.set_string(key, value) {
                            warn!("error writing {store} {key}: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Write a boolean key on one of the stores. Missing store/key is a no-op.
    fn set_bool(&self, store: &'static str, key: &'static str, value: bool) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(settings) = s.get(store) {
                    if settings_has_key(settings, key) {
                        if let Err(err) = settings.set_boolean(key, value) {
                            warn!("error writing {store} {key}: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }
}

/// The `org.gnome.mutter.keybindings` keys we honor, with their schema
/// defaults (this schema ships with mutter, so the defaults are authoritative
/// in the reference checkout).
fn adopted_mutter_keybindings() -> Vec<(String, GnomeKeyAction, Vec<String>)> {
    vec![
        (
            "toggle-tiled-left".to_owned(),
            GnomeKeyAction::ToggleTiled(TileSide::Left),
            vec!["<Super>Left".to_owned()],
        ),
        (
            "toggle-tiled-right".to_owned(),
            GnomeKeyAction::ToggleTiled(TileSide::Right),
            vec!["<Super>Right".to_owned()],
        ),
    ]
}

/// The GSettings stores feeding [`GnomeSettings`]. Any change to any of them
/// re-reads the whole model — settings churn is rare and the read is cheap,
/// and one code path means one behavior to test.
struct Stores {
    mutter: Option<gio::Settings>,
    mutter_keybindings: Option<gio::Settings>,
    wm_keybindings: Option<gio::Settings>,
    wm_preferences: Option<gio::Settings>,
    shell: Option<gio::Settings>,
    lockdown: Option<gio::Settings>,
    background: Option<gio::Settings>,
    interface: Option<gio::Settings>,
    calendar: Option<gio::Settings>,
    notifications: Option<gio::Settings>,
    color: Option<gio::Settings>,
    input_sources: Option<gio::Settings>,
    world_clocks: Option<gio::Settings>,
    /// Whether `org.gnome.clocks.desktop` is installed, sampled once at open.
    clocks_installed: bool,
    /// Cache for the expensive coordinate→timezone resolution, keyed by the parsed
    /// locations: only rebuild the `tzf-rs` finder when the blob actually changes,
    /// not on every unrelated settings change that re-reads the model.
    world_clocks_cache: RefCell<WorldClocksCache>,
    /// The GNOME Clocks D-Bus proxy driving the `locations`-mirror; held alive for
    /// the watcher thread's lifetime once set up.
    clocks_proxy: RefCell<Option<gio::DBusProxy>>,
}

impl Stores {
    /// Open every schema we honor; each is `None` where not installed (e.g.
    /// running outside a GNOME environment).
    fn open() -> Self {
        Self {
            mutter: gsettings("org.gnome.mutter"),
            mutter_keybindings: gsettings("org.gnome.mutter.keybindings"),
            wm_keybindings: gsettings("org.gnome.desktop.wm.keybindings"),
            wm_preferences: gsettings("org.gnome.desktop.wm.preferences"),
            shell: gsettings("org.gnome.shell"),
            lockdown: gsettings("org.gnome.desktop.lockdown"),
            background: gsettings("org.gnome.desktop.background"),
            interface: gsettings("org.gnome.desktop.interface"),
            calendar: gsettings("org.gnome.desktop.calendar"),
            notifications: gsettings("org.gnome.desktop.notifications"),
            color: gsettings("org.gnome.settings-daemon.plugins.color"),
            input_sources: gsettings("org.gnome.desktop.input-sources"),
            world_clocks: gsettings("org.gnome.shell.world-clocks"),
            clocks_installed: desktop_app_installed("org.gnome.clocks.desktop"),
            world_clocks_cache: RefCell::new(None),
            clocks_proxy: RefCell::new(None),
        }
    }

    fn any(&self) -> bool {
        self.all().next().is_some()
    }

    /// The open store with this short name, for [`GnomeSettingsWriter`]'s setters.
    fn get(&self, name: &str) -> Option<&gio::Settings> {
        match name {
            "interface" => self.interface.as_ref(),
            "notifications" => self.notifications.as_ref(),
            "color" => self.color.as_ref(),
            "shell" => self.shell.as_ref(),
            "input-sources" => self.input_sources.as_ref(),
            _ => None,
        }
    }

    fn all(&self) -> impl Iterator<Item = &gio::Settings> {
        [
            &self.mutter,
            &self.mutter_keybindings,
            &self.wm_keybindings,
            &self.wm_preferences,
            &self.shell,
            &self.lockdown,
            &self.background,
            &self.interface,
            &self.calendar,
            &self.notifications,
            &self.color,
            &self.input_sources,
            &self.world_clocks,
        ]
        .into_iter()
        .flatten()
    }

    /// GNOME's defaults overlaid with the live values of every open store.
    fn read(&self) -> GnomeSettings {
        let mut settings = GnomeSettings::default();
        if let Some(mutter) = &self.mutter {
            settings.load_mutter(mutter);
        }
        settings.load_keybindings(
            self.wm_keybindings.as_ref(),
            self.mutter_keybindings.as_ref(),
        );
        if let Some(wm) = &self.wm_preferences {
            settings.load_wm_preferences(wm);
        }
        if let Some(shell) = &self.shell {
            settings.load_shell(shell);
        }
        if let Some(lockdown) = &self.lockdown {
            settings.load_lockdown(lockdown);
        }
        if let Some(background) = &self.background {
            settings.load_background(background, self.interface.as_ref());
        }
        if let Some(interface) = &self.interface {
            settings.load_interface(interface);
        }
        if let Some(calendar) = &self.calendar {
            settings.load_calendar(calendar);
        }
        if let Some(notifications) = &self.notifications {
            settings.load_notifications(notifications);
        }
        if let Some(color) = &self.color {
            settings.load_color(color);
        }
        if let Some(input_sources) = &self.input_sources {
            settings.load_input_sources(input_sources);
        }
        settings.world_clocks.clocks_installed = self.clocks_installed;
        if let Some(world_clocks) = &self.world_clocks {
            settings.load_world_clocks(world_clocks, &self.world_clocks_cache);
        }
        settings
    }

    /// Invoke `on_change` with a freshly-read model whenever any key in any
    /// store changes. The subscriptions live as long as the stores do.
    fn subscribe(self: &Rc<Self>, on_change: impl Fn(GnomeSettings) + 'static) {
        let on_change = Rc::new(on_change);
        for settings in self.all() {
            let stores = self.clone();
            let on_change = on_change.clone();
            settings.connect_changed(None, move |_, _key| {
                on_change(stores.read());
            });
        }
    }

    /// Mirror GNOME Clocks' saved locations into `org.gnome.shell.world-clocks`,
    /// gnome-shell's `WorldClocksSection._onClocksPropertiesChanged`
    /// (`dateMenu.js:523-540`). Clocks exports its world clocks as the
    /// `Locations` (`av`) property of `org.gnome.Shell.ClocksIntegration`; the
    /// shell is the only writer of the gsettings key, so without this the section
    /// never populates on a system that never ran stock gnome-shell.
    ///
    /// The write re-enters this store's `changed` subscription, so the model is
    /// refreshed like any external change. Runs on the watcher thread's glib main
    /// context (like GNOME's `Gio.DBusProxy`); the proxy is kept alive in `self`.
    fn setup_clocks_mirror(self: &Rc<Self>) {
        let Some(world_clocks) = self.world_clocks.clone() else {
            return;
        };
        // DO_NOT_AUTO_START: an absent Clocks just leaves the last-mirrored value.
        let proxy = match gio::DBusProxy::for_bus_sync(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START
                | gio::DBusProxyFlags::GET_INVALIDATED_PROPERTIES,
            None,
            "org.gnome.clocks",
            "/org/gnome/clocks",
            "org.gnome.Shell.ClocksIntegration",
            gio::Cancellable::NONE,
        ) {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("could not create GNOME Clocks proxy for the world-clocks mirror: {err}");
                return;
            }
        };

        let mirror = Rc::new(move |proxy: &gio::DBusProxy| {
            // Only mirror while Clocks actually owns the name — copying an empty
            // `Locations` from a nascent/absent proxy would wipe the saved clocks
            // (`dateMenu.js:534-536`).
            if proxy.name_owner().is_none() {
                return;
            }
            if let Some(locations) = proxy.cached_property("Locations") {
                if let Err(err) = world_clocks.set_value("locations", &locations) {
                    warn!("error mirroring GNOME Clocks locations: {err}");
                }
            }
        });

        let on_change = mirror.clone();
        // `connect_local` (not the `Send + Sync` `connect_g_properties_changed`): the
        // proxy and its callback live only on this glib thread.
        proxy.connect_local("g-properties-changed", false, move |values| {
            if let Ok(proxy) = values[0].get::<gio::DBusProxy>() {
                on_change(&proxy);
            }
            None
        });
        // The sync constructor loads properties immediately: mirror once now (covers
        // a Clocks already running at startup); later changes/owner transitions
        // re-emit `g-properties-changed`.
        mirror(&proxy);
        self.clocks_proxy.replace(Some(proxy));
    }
}

/// Open a [`gio::Settings`] for `schema_id`, or `None` if the schema isn't
/// installed (e.g. running outside a GNOME environment). Guarding with the schema
/// source avoids `gio::Settings::new`'s abort-on-missing-schema behavior.
fn gsettings(schema_id: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema_id, true)?;
    Some(gio::Settings::new(schema_id))
}

/// Whether an application `${id}.desktop` is installed, gnome-shell's
/// `Shell.AppSystem.lookup_app` as used by `WorldClocksSection._sync`. Scans the
/// XDG `applications` dirs for a flat `<id>` file (nested / vendor-prefixed
/// desktop-id resolution is a recorded divergence — fine for `org.gnome.clocks`).
fn desktop_app_installed(desktop_id: &str) -> bool {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .unwrap_or_else(|| std::ffi::OsString::from("/usr/local/share:/usr/share"));
    dirs.extend(std::env::split_paths(&data_dirs));
    dirs.iter()
        .any(|dir| dir.join("applications").join(desktop_id).exists())
}

bitflags::bitflags! {
    /// Modifiers of a parsed accelerator, mirroring the masks mutter's
    /// `meta_parse_accelerator` produces: `MOD1` is the Alt mask (`<Alt>` and
    /// `<Mod1>` are the same bit); `META`/`HYPER`/`SUPER` are virtual
    /// modifiers mutter resolves through the keymap's modmap (conventionally
    /// Meta lives on the Alt keys and Hyper on the Super keys).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AccelMods: u16 {
        const SHIFT = 1 << 0;
        const CONTROL = 1 << 1;
        const MOD1 = 1 << 2;
        const MOD2 = 1 << 3;
        const MOD3 = 1 << 4;
        const MOD4 = 1 << 5;
        const MOD5 = 1 << 6;
        const META = 1 << 7;
        const HYPER = 1 << 8;
        const SUPER = 1 << 9;
    }
}

/// What a parsed accelerator triggers on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelTrigger {
    /// A keysym name, resolved case-insensitively.
    Keysym(Keysym),
    /// A hardware (xkb) keycode: the `0xNN` syntax, and `Above_Tab`, which
    /// mutter resolves straight to the key above Tab regardless of layout.
    Keycode(u32),
}

/// One parsed keyboard accelerator — a single entry of a keybinding's
/// GSettings array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accel {
    pub trigger: AccelTrigger,
    pub mods: AccelMods,
}

/// `Above_Tab` resolves to the physical key above Tab: evdev `KEY_GRAVE`
/// (0x29) plus the xkb keycode offset (mutter: `get_above_tab_keycode`).
const ABOVE_TAB_KEYCODE: u32 = 0x29 + 8;

/// A live external accelerator grab (`org.gnome.Shell` `GrabAccelerator`),
/// mirroring mutter's `MetaKeyGrab`: gsd-media-keys and friends register
/// their key combos here and get an `AcceleratorActivated` D-Bus signal when
/// one fires, instead of the compositor acting on the key itself.
#[derive(Debug, Clone, PartialEq)]
pub struct AccelGrab {
    /// The dynamic action id handed back to the grabber; nonzero.
    pub action: u32,
    pub accel: Accel,
    /// `Shell.ActionMode` bitmask: when the grab may fire. We only
    /// distinguish "usable while locked" (LOCK_SCREEN | UNLOCK_SCREEN).
    pub mode_flags: u32,
    /// `MetaKeyBindingFlags`: NON_MASKABLE and IGNORE_AUTOREPEAT are honored.
    pub grab_flags: u32,
    /// Unique D-Bus name of the grabber, for unicast signals and cleanup.
    pub owner: String,
}

impl AccelGrab {
    pub const MODE_LOCK_SCREEN: u32 = 1 << 2;
    pub const MODE_UNLOCK_SCREEN: u32 = 1 << 3;
    pub const FLAG_NON_MASKABLE: u32 = 1 << 3;
    pub const FLAG_IGNORE_AUTOREPEAT: u32 = 1 << 4;
}

/// Parse one accelerator string with mutter's grammar (`meta_parse_accelerator`
/// in `meta-accel-parse.c`):
///
/// - empty or the literal `"disabled"` → `Ok(None)`: valid but unbound; so is an accelerator with
///   modifiers and no key (mutter's zero combo);
/// - leading `<Token>` modifiers, case-insensitive: `<Shift>`/`<Shft>`,
///   `<Control>`/`<Ctrl>`/`<Ctl>`/`<Primary>`, `<Alt>`, `<Meta>`, `<Hyper>`, `<Super>`,
///   `<Mod1>`–`<Mod5>`; an unrecognized `<...>` token is silently skipped, exactly like mutter;
/// - the rest is the key: `0x` + hex digits (a raw keycode; trailing junk tolerated), the exact
///   string `Above_Tab`, or a keysym name resolved case-insensitively with an `XF86` prefix retry
///   (so `AudioPlay` works);
/// - an unresolvable key → `Err(())`.
pub(crate) fn parse_accelerator(accel: &str) -> Result<Option<Accel>, ()> {
    if accel.is_empty() || accel == "disabled" {
        return Ok(None);
    }

    let mut mods = AccelMods::empty();
    let mut rest = accel;
    while let Some(after) = rest.strip_prefix('<') {
        let Some((token, after)) = after.split_once('>') else {
            return Err(());
        };
        mods |= match token.to_ascii_lowercase().as_str() {
            "shift" | "shft" => AccelMods::SHIFT,
            "control" | "ctrl" | "ctl" | "primary" => AccelMods::CONTROL,
            "alt" | "mod1" => AccelMods::MOD1,
            "mod2" => AccelMods::MOD2,
            "mod3" => AccelMods::MOD3,
            "mod4" => AccelMods::MOD4,
            "mod5" => AccelMods::MOD5,
            "meta" => AccelMods::META,
            "hyper" => AccelMods::HYPER,
            "super" => AccelMods::SUPER,
            _ => AccelMods::empty(),
        };
        rest = after;
    }

    if rest.is_empty() {
        return Ok(None);
    }

    let trigger = if let Some(keycode) = parse_accel_keycode(rest) {
        AccelTrigger::Keycode(keycode)
    } else if rest == "Above_Tab" {
        AccelTrigger::Keycode(ABOVE_TAB_KEYCODE)
    } else {
        let mut keysym = xkb::keysym_from_name(rest, xkb::KEYSYM_CASE_INSENSITIVE);
        if keysym == Keysym::NoSymbol {
            keysym = xkb::keysym_from_name(&format!("XF86{rest}"), xkb::KEYSYM_CASE_INSENSITIVE);
        }
        if keysym == Keysym::NoSymbol {
            return Err(());
        }
        AccelTrigger::Keysym(keysym)
    };

    Ok(Some(Accel { trigger, mods }))
}

/// Mutter's `is_keycode` + `strtoul`: `0x` and at least two hex digits;
/// parsing stops at the first non-hex character.
fn parse_accel_keycode(s: &str) -> Option<u32> {
    let hex = s.strip_prefix("0x")?;
    let end = hex
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hex.len());
    if end < 2 {
        return None;
    }
    u32::from_str_radix(&hex[..end], 16).ok()
}

/// Parse a GNOME `overlay-key`-style value into the set of trigger keysyms,
/// reproducing mutter's `parse_special_key`/`meta_parse_accelerator`:
///
/// - empty or the literal `"disabled"` → disabled (`Ok(vec![])`);
/// - a recognized keysym name → that one key (e.g. `"Menu"`);
/// - otherwise the bare modifier form, expanded to its `_L`/`_R` pair (e.g. GNOME's default
///   `"Super"` → `Super_L` + `Super_R`);
/// - anything else → `Err(name)` so the caller can warn and keep the default.
fn parse_overlay_key(name: &str) -> Result<Vec<Keysym>, &str> {
    if name.is_empty() || name == "disabled" {
        return Ok(Vec::new());
    }

    let keysym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
    if keysym != Keysym::NoSymbol {
        return Ok(vec![keysym]);
    }

    // A bare modifier name like "Super" isn't itself a keysym; mutter retries it
    // as the left/right pair, and only accepts it if both resolve.
    let left = xkb::keysym_from_name(&format!("{name}_L"), xkb::KEYSYM_NO_FLAGS);
    let right = xkb::keysym_from_name(&format!("{name}_R"), xkb::KEYSYM_NO_FLAGS);
    if left != Keysym::NoSymbol && right != Keysym::NoSymbol {
        Ok(vec![left, right])
    } else {
        Err(name)
    }
}

/// Parse a `picture-options` value; an unrecognized one falls back to the
/// schema default (`zoom`) with a warning, like a fresh GNOME install shows.
fn parse_picture_options(value: &str) -> BackgroundOptions {
    match value {
        "none" => BackgroundOptions::None,
        "wallpaper" => BackgroundOptions::Wallpaper,
        "centered" => BackgroundOptions::Centered,
        "scaled" => BackgroundOptions::Scaled,
        "stretched" => BackgroundOptions::Stretched,
        "zoom" => BackgroundOptions::Zoom,
        "spanned" => BackgroundOptions::Spanned,
        other => {
            warn!("ignoring unrecognized picture-options {other:?}; using zoom");
            BackgroundOptions::Zoom
        }
    }
}

/// gnome-shell's accent palette (st-theme-context.c `ACCENT_COLOR_*`),
/// keyed by the `org.gnome.desktop.interface accent-color` enum values.
fn parse_accent_color(name: &str) -> Option<[u8; 3]> {
    Some(match name {
        "blue" => ACCENT_BLUE,
        "teal" => [0x21, 0x90, 0xa4],
        "green" => [0x3a, 0x94, 0x4a],
        "yellow" => [0xc8, 0x88, 0x00],
        "orange" => [0xed, 0x5b, 0x00],
        "red" => [0xe6, 0x2d, 0x42],
        "pink" => [0xd5, 0x61, 0x99],
        "purple" => [0x91, 0x41, 0xac],
        "slate" => [0x6f, 0x83, 0x96],
        _ => return None,
    })
}

/// Resolve a `picture-uri` value to the local file to decode. `None` (no
/// picture) when the URI is empty, isn't a local `file://` URI, or the
/// options say not to draw it.
fn resolve_picture_uri(uri: &str, options: BackgroundOptions) -> Option<PathBuf> {
    if uri.is_empty() || options == BackgroundOptions::None {
        return None;
    }
    match glib::filename_from_uri(uri) {
        Ok((path, _host)) => Some(path),
        Err(err) => {
            warn!("ignoring non-local background picture-uri {uri:?}: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: `locale_week_start` must read the weekday item, not a month name. The historic
    /// bug used item `0x2000e` (`ABMON_1`, "Jan"…), whose first byte is a letter (≥ 'A' = 65), so
    /// `(byte-1)%7` silently yielded Wednesday for every locale. The correct
    /// `_NL_TIME_FIRST_WEEKDAY` (`0x20068`) returns a small weekday code 1..=7. Assert the
    /// constant reads a sane code, and the derived week start is a valid 0..=6.
    #[test]
    fn week_start_reads_weekday_not_month_name() {
        const _NL_TIME_FIRST_WEEKDAY: libc::nl_item = 0x20068;
        // SAFETY: nl_langinfo returns a static, NUL-terminated string; read its first byte.
        let byte = unsafe {
            let p = libc::nl_langinfo(_NL_TIME_FIRST_WEEKDAY);
            assert!(!p.is_null(), "nl_langinfo returned null");
            *p as u8
        };
        assert!(
            (1..=7).contains(&byte),
            "first-weekday byte {byte} is not a weekday code 1..=7 — wrong langinfo item \
             (a month-name byte, the old ABMON_1 bug, would be a letter ≥ 65)"
        );
        assert!(locale_week_start() <= 6);
    }

    #[test]
    fn read_source_tuples_unpacks_ass() {
        use glib::prelude::ToVariant;

        // `sources` / `mru-sources` are `a(ss)` = (type, id) pairs.
        let variant = vec![
            ("xkb".to_string(), "us".to_string()),
            ("xkb".to_string(), "de+nodeadkeys".to_string()),
            ("ibus".to_string(), "libpinyin".to_string()),
        ]
        .to_variant();
        assert_eq!(variant.type_().as_str(), "a(ss)");
        assert_eq!(
            read_source_tuples(&variant),
            vec![
                ("xkb".to_string(), "us".to_string()),
                ("xkb".to_string(), "de+nodeadkeys".to_string()),
                ("ibus".to_string(), "libpinyin".to_string()),
            ]
        );
        let empty = Vec::<(String, String)>::new().to_variant();
        assert!(read_source_tuples(&empty).is_empty());
    }

    #[test]
    fn accent_colors_follow_the_shell_palette() {
        // st-theme-context.c ACCENT_COLOR_*.
        assert_eq!(parse_accent_color("blue"), Some([0x35, 0x84, 0xe4]));
        assert_eq!(parse_accent_color("teal"), Some([0x21, 0x90, 0xa4]));
        assert_eq!(parse_accent_color("slate"), Some([0x6f, 0x83, 0x96]));
        assert_eq!(parse_accent_color("chartreuse"), None);
    }

    #[test]
    fn picture_uri_resolves_to_local_path() {
        assert_eq!(
            resolve_picture_uri(
                "file:///usr/share/backgrounds/gnome/adwaita-l.jxl",
                BackgroundOptions::Zoom,
            ),
            Some(PathBuf::from("/usr/share/backgrounds/gnome/adwaita-l.jxl"))
        );
        // URI-encoding is decoded on the way.
        assert_eq!(
            resolve_picture_uri(
                "file:///home/user/my%20wallpaper.png",
                BackgroundOptions::Zoom
            ),
            Some(PathBuf::from("/home/user/my wallpaper.png"))
        );
    }

    #[test]
    fn picture_uri_rejects_empty_remote_and_none_options() {
        assert_eq!(resolve_picture_uri("", BackgroundOptions::Zoom), None);
        assert_eq!(
            resolve_picture_uri("https://example.com/wall.png", BackgroundOptions::Zoom),
            None
        );
        // picture-options=none means "no picture", whatever the URI says.
        assert_eq!(
            resolve_picture_uri("file:///tmp/wall.png", BackgroundOptions::None),
            None
        );
    }

    #[test]
    fn parse_overlay_key_bare_super_is_both() {
        // GNOME's default value, the reason this returns a set rather than one key.
        assert_eq!(
            parse_overlay_key("Super"),
            Ok(vec![Keysym::Super_L, Keysym::Super_R])
        );
    }

    #[test]
    fn parse_overlay_key_explicit_names() {
        assert_eq!(parse_overlay_key("Super_L"), Ok(vec![Keysym::Super_L]));
        assert_eq!(parse_overlay_key("Super_R"), Ok(vec![Keysym::Super_R]));
        assert_eq!(parse_overlay_key("Menu"), Ok(vec![Keysym::Menu]));
    }

    #[test]
    fn parse_overlay_key_disabled() {
        assert_eq!(parse_overlay_key(""), Ok(Vec::new()));
        assert_eq!(parse_overlay_key("disabled"), Ok(Vec::new()));
    }

    #[test]
    fn parse_overlay_key_garbage_is_rejected() {
        assert_eq!(
            parse_overlay_key("definitely not a keysym"),
            Err("definitely not a keysym")
        );
    }

    #[test]
    fn parse_accelerator_plain_and_modified() {
        assert_eq!(
            parse_accelerator("<Alt>F2"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keysym(Keysym::F2),
                mods: AccelMods::MOD1,
            }))
        );
        assert_eq!(
            parse_accelerator("<Super><Shift>Page_Up"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keysym(Keysym::Page_Up),
                mods: AccelMods::SUPER | AccelMods::SHIFT,
            }))
        );
        assert_eq!(
            parse_accelerator("p"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keysym(Keysym::p),
                mods: AccelMods::empty(),
            }))
        );
    }

    #[test]
    fn parse_accelerator_tokens_are_case_insensitive_with_aliases() {
        // <Primary> is Control; <Ctl>/<Ctrl> and <Shft> are accepted spellings;
        // token case doesn't matter (mutter's is_*() helpers fold case).
        for accel in ["<Primary><Alt>F1", "<control><alt>F1", "<CTL><MOD1>F1"] {
            assert_eq!(
                parse_accelerator(accel),
                Ok(Some(Accel {
                    trigger: AccelTrigger::Keysym(Keysym::F1),
                    mods: AccelMods::CONTROL | AccelMods::MOD1,
                })),
                "{accel:?} must parse as Control+Alt+F1"
            );
        }
        assert_eq!(
            parse_accelerator("<Shft>q").unwrap().unwrap().mods,
            AccelMods::SHIFT
        );
    }

    #[test]
    fn parse_accelerator_unknown_token_is_skipped() {
        // Mutter silently ignores unrecognized <...> tokens.
        assert_eq!(
            parse_accelerator("<Bogus>t"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keysym(Keysym::t),
                mods: AccelMods::empty(),
            }))
        );
    }

    #[test]
    fn parse_accelerator_xf86_retry() {
        // "AudioPlay" is not a keysym; mutter retries with the XF86 prefix.
        assert_eq!(
            parse_accelerator("AudioPlay"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keysym(Keysym::XF86_AudioPlay),
                mods: AccelMods::empty(),
            }))
        );
    }

    #[test]
    fn parse_accelerator_keycode_and_above_tab() {
        assert_eq!(
            parse_accelerator("<Super>0x29"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keycode(0x29),
                mods: AccelMods::SUPER,
            }))
        );
        assert_eq!(
            parse_accelerator("<Alt>Above_Tab"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keycode(ABOVE_TAB_KEYCODE),
                mods: AccelMods::MOD1,
            }))
        );
        // One hex digit doesn't satisfy is_keycode — and then falls into the
        // keysym lookup, where xkbcommon accepts hex strings as *numeric
        // keysyms*. Mutter takes the same path, so this quirk is faithful.
        assert_eq!(
            parse_accelerator("0x2"),
            Ok(Some(Accel {
                trigger: AccelTrigger::Keysym(Keysym::new(0x2)),
                mods: AccelMods::empty(),
            }))
        );
    }

    #[test]
    fn parse_accelerator_disabled_and_invalid() {
        assert_eq!(parse_accelerator(""), Ok(None));
        assert_eq!(parse_accelerator("disabled"), Ok(None));
        // Modifiers without a key: valid, but never fires (mutter zero combo).
        assert_eq!(parse_accelerator("<Super>"), Ok(None));
        assert_eq!(parse_accelerator("definitely not a keysym"), Err(()));
    }

    #[test]
    fn default_keybindings_include_the_gnome_session_defaults() {
        let settings = GnomeSettings::default();
        let accels_of = |action: GnomeKeyAction| {
            settings
                .keybindings
                .iter()
                .filter(|kb| kb.action == action)
                .flat_map(|kb| kb.accels.clone())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            accels_of(GnomeKeyAction::Close),
            vec![Accel {
                trigger: AccelTrigger::Keysym(Keysym::F4),
                mods: AccelMods::MOD1,
            }],
            "close defaults to <Alt>F4"
        );
        assert_eq!(
            accels_of(GnomeKeyAction::PanelRunDialog),
            vec![Accel {
                trigger: AccelTrigger::Keysym(Keysym::F2),
                mods: AccelMods::MOD1,
            }],
            "panel-run-dialog defaults to <Alt>F2"
        );
        assert_eq!(
            accels_of(GnomeKeyAction::SwitchToWorkspace(2)),
            vec![Accel {
                trigger: AccelTrigger::Keysym(Keysym::_2),
                mods: AccelMods::SUPER,
            }],
            "switch-to-workspace-2 defaults to <Super>2"
        );
        // Both directional axes funnel into previous/next.
        assert_eq!(
            accels_of(GnomeKeyAction::SwitchToWorkspaceNext).len(),
            5,
            "right (4 accels) + down (1 accel) collapse onto next"
        );
    }

    /// The change subscription re-reads the model when a key in any watched
    /// store is written. Uses a memory settings backend so nothing touches the
    /// user's real dconf, and a private main context standing in for the
    /// watcher thread's.
    #[test]
    fn settings_change_subscription_delivers_updates() {
        use std::cell::RefCell;

        // The schemas come from the host system; skip where not installed.
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return;
        };
        let Some(mutter_schema) = source.lookup("org.gnome.mutter", true) else {
            return;
        };
        let Some(wm_schema) = source.lookup("org.gnome.desktop.wm.keybindings", true) else {
            return;
        };

        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            let backend = gio::memory_settings_backend_new();
            let stores = Rc::new(Stores {
                mutter: Some(gio::Settings::new_full(
                    &mutter_schema,
                    Some(&backend),
                    None,
                )),
                mutter_keybindings: None,
                wm_keybindings: Some(gio::Settings::new_full(&wm_schema, Some(&backend), None)),
                wm_preferences: None,
                shell: None,
                lockdown: None,
                background: None,
                interface: None,
                calendar: None,
                notifications: None,
                color: None,
                input_sources: None,
                world_clocks: None,
                clocks_installed: false,
                world_clocks_cache: RefCell::new(None),
                clocks_proxy: RefCell::new(None),
            });

            let received = Rc::new(RefCell::new(Vec::new()));
            stores.subscribe({
                let received = received.clone();
                move |settings| received.borrow_mut().push(settings)
            });
            let settle = || {
                while ctx.pending() {
                    ctx.iteration(false);
                }
            };

            stores
                .mutter
                .as_ref()
                .unwrap()
                .set_string("overlay-key", "Menu")
                .unwrap();
            settle();
            assert_eq!(
                received.borrow().last().map(|s| s.overlay_keys.clone()),
                Some(vec![Keysym::Menu]),
                "a write to overlay-key must deliver a re-read model"
            );

            stores
                .wm_keybindings
                .as_ref()
                .unwrap()
                .set_strv("close", ["<Super>w"])
                .unwrap();
            settle();
            let received = received.borrow();
            let close = received
                .last()
                .and_then(|s| {
                    s.keybindings
                        .iter()
                        .find(|kb| kb.action == GnomeKeyAction::Close)
                })
                .map(|kb| kb.accels.clone());
            assert_eq!(
                close,
                Some(vec![Accel {
                    trigger: AccelTrigger::Keysym(Keysym::w),
                    mods: AccelMods::SUPER,
                }]),
                "a write to a wm keybinding must deliver a re-read model"
            );
        })
        .unwrap();
    }

    /// [`GnomeSettingsWriter`] hops onto the watcher thread and lands the
    /// write in the store. This drives the real writer against a dedicated
    /// thread running a glib loop, with a memory backend standing in for
    /// dconf, and reads the value back from that same thread.
    #[test]
    fn writer_persists_command_history() {
        // The schema comes from the host system; skip where not installed.
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return;
        };
        let Some(shell_schema) = source.lookup("org.gnome.shell", true) else {
            return;
        };
        if !shell_schema.has_key("command-history") {
            return;
        }

        let ctx = glib::MainContext::new();
        let writer = GnomeSettingsWriter { ctx: ctx.clone() };

        let (loop_tx, loop_rx) = std::sync::mpsc::channel();
        let watcher = std::thread::spawn({
            let ctx = ctx.clone();
            move || {
                ctx.with_thread_default(|| {
                    // SettingsSchema is not Send; look it up again here.
                    let shell_schema = gio::SettingsSchemaSource::default()
                        .unwrap()
                        .lookup("org.gnome.shell", true)
                        .unwrap();
                    let backend = gio::memory_settings_backend_new();
                    let shell = gio::Settings::new_full(&shell_schema, Some(&backend), None);
                    STORES.set(Some(Rc::new(Stores {
                        mutter: None,
                        mutter_keybindings: None,
                        wm_keybindings: None,
                        wm_preferences: None,
                        shell: Some(shell),
                        lockdown: None,
                        background: None,
                        interface: None,
                        calendar: None,
                        notifications: None,
                        color: None,
                        input_sources: None,
                        world_clocks: None,
                        clocks_installed: false,
                        world_clocks_cache: RefCell::new(None),
                        clocks_proxy: RefCell::new(None),
                    })));

                    let main_loop = glib::MainLoop::new(Some(&ctx), false);
                    loop_tx.send(main_loop.clone()).unwrap();
                    main_loop.run();
                })
                .unwrap();
            }
        });
        let main_loop = loop_rx.recv().unwrap();

        writer.set_command_history(vec!["echo hi".to_owned()]);

        // Invokes run in order, so this reads back after the write landed.
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        ctx.invoke(move || {
            STORES.with(|stores| {
                let s = stores.take().unwrap();
                let history: Vec<String> = s
                    .shell
                    .as_ref()
                    .unwrap()
                    .strv("command-history")
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                read_tx.send(history).unwrap();
                stores.set(Some(s));
            });
        });

        assert_eq!(
            read_rx.recv().unwrap(),
            vec!["echo hi".to_owned()],
            "the write must land in the store via the watcher thread"
        );

        main_loop.quit();
        watcher.join().unwrap();
    }

    #[test]
    fn writer_persists_favorite_apps() {
        // The schema comes from the host system; skip where not installed.
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return;
        };
        let Some(shell_schema) = source.lookup("org.gnome.shell", true) else {
            return;
        };
        if !shell_schema.has_key("favorite-apps") {
            return;
        }

        let ctx = glib::MainContext::new();
        let writer = GnomeSettingsWriter { ctx: ctx.clone() };

        let (loop_tx, loop_rx) = std::sync::mpsc::channel();
        let watcher = std::thread::spawn({
            let ctx = ctx.clone();
            move || {
                ctx.with_thread_default(|| {
                    // SettingsSchema is not Send; look it up again here.
                    let shell_schema = gio::SettingsSchemaSource::default()
                        .unwrap()
                        .lookup("org.gnome.shell", true)
                        .unwrap();
                    let backend = gio::memory_settings_backend_new();
                    let shell = gio::Settings::new_full(&shell_schema, Some(&backend), None);
                    STORES.set(Some(Rc::new(Stores {
                        mutter: None,
                        mutter_keybindings: None,
                        wm_keybindings: None,
                        wm_preferences: None,
                        shell: Some(shell),
                        lockdown: None,
                        background: None,
                        interface: None,
                        calendar: None,
                        notifications: None,
                        color: None,
                        input_sources: None,
                        world_clocks: None,
                        clocks_installed: false,
                        world_clocks_cache: RefCell::new(None),
                        clocks_proxy: RefCell::new(None),
                    })));

                    let main_loop = glib::MainLoop::new(Some(&ctx), false);
                    loop_tx.send(main_loop.clone()).unwrap();
                    main_loop.run();
                })
                .unwrap();
            }
        });
        let main_loop = loop_rx.recv().unwrap();

        writer.set_favorite_apps(vec!["org.gnome.Nautilus.desktop".to_owned()]);

        // Invokes run in order, so this reads back after the write landed.
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        ctx.invoke(move || {
            STORES.with(|stores| {
                let s = stores.take().unwrap();
                let favorites: Vec<String> = s
                    .shell
                    .as_ref()
                    .unwrap()
                    .strv("favorite-apps")
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                read_tx.send(favorites).unwrap();
                stores.set(Some(s));
            });
        });

        assert_eq!(
            read_rx.recv().unwrap(),
            vec!["org.gnome.Nautilus.desktop".to_owned()],
            "the write must land in the store via the watcher thread"
        );

        main_loop.quit();
        watcher.join().unwrap();
    }
}
