// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Fork-owned GNOME desktop policy.
//!
//! This module holds the inspectable model of the GNOME *settings* and policy the
//! compositor honors, kept deliberately separate from niri's own TOML config and
//! from the per-frame render path (see `docs/fork/STRATEGY.md`). GNOME policy
//! state flows through here as one inspectable struct rather than being scattered
//! across the input/render code.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gio::glib;
use gio::glib::prelude::ObjectExt;
use gio::prelude::{DBusProxyExt, SettingsExt, SettingsExtManual};
use smithay::input::keyboard::{xkb, Keysym};
use synoik_config::{Action, Modifiers};
use synoik_ipc::SizeChange;

use crate::input::peripherals::Peripherals;
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
/// The Alt-Tab / Super-Tab switcher settings — `org.gnome.shell.app-switcher` and
/// `org.gnome.shell.window-switcher` (`data/org.gnome.shell.gschema.xml.in:307-343`).
///
/// One struct for two schemas on purpose: they share a key *name* whose default is the
/// **opposite** in each, so stock Super-Tab spans workspaces and stock Alt-Tab does not. Reading
/// one where the other was meant is invisible until someone uses a second workspace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwitcherSettings {
    /// `app-switcher current-workspace-only` — default **false**.
    pub apps_current_workspace_only: bool,
    /// `window-switcher current-workspace-only` — default **true**.
    pub windows_current_workspace_only: bool,
}

impl Default for SwitcherSettings {
    fn default() -> Self {
        Self {
            apps_current_workspace_only: false,
            windows_current_workspace_only: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    /// The Alt-Tab / Super-Tab switcher settings — two schemas whose same-named key has
    /// **opposite** defaults, so they are kept as one struct to make that hard to miss.
    pub switchers: SwitcherSettings,
    /// `org.gnome.desktop.lockdown disable-command-line`: when set, the run
    /// dialog refuses to open (gnome-shell's `RunDialog.open`).
    pub disable_command_line: bool,
    /// The screen shield's two keys — `disable-lock-screen` (lockdown) and
    /// `lock-enabled` (screensaver). Kept as one struct because the shield
    /// consults them together and they live in different schemas.
    pub shield: crate::screen_shield::ShieldSettings,
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
    /// `org.gnome.shell app-picker-layout`: the user's saved app-grid arrangement, as
    /// `(page, position)` per desktop id — what the grid orders by
    /// (`PageManager.getAppPosition` + `AppDisplay._compareItems`,
    /// `appDisplay.js:1276-1291,1475-1490`). Empty until the user rearranges the grid,
    /// in which case everything falls back to the by-name order.
    ///
    /// Folder ids appear here too (`"Utilities"`); they resolve to no app and are simply
    /// not found when an id is looked up.
    pub app_picker_layout: HashMap<String, (usize, i32)>,
    /// `org.gnome.desktop.interface font-name`'s point size — the **realized** base
    /// every theme point size is a ratio against. See [`crate::ui::base_font_pt`] for
    /// why the theme's own `$base_font_size` is only nominal. GNOME's default is 11.
    pub base_font_pt: f64,

    /// `org.gnome.desktop.interface font-name`'s family — the other half of the same key,
    /// realized the same way. See [`synoik_vk::text::sans_family`].
    pub base_font_family: String,
    /// `org.gnome.desktop.interface gtk-key-theme`: which editing bindings text entries
    /// honor. See [`crate::ui::text_edit`] — GNOME Shell itself ignores this key (it is a
    /// GTK mechanism), and honoring it in our entries is a deliberate divergence.
    pub key_theme: crate::ui::text_edit::KeyTheme,
    /// `org.gnome.desktop.interface icon-theme`: the icon theme both the symbolic
    /// icon cache and the app-icon loader resolve against. GNOME's default is
    /// `"Adwaita"`.
    pub icon_theme: String,

    /// `org.gnome.desktop.interface enable-animations`. We do not gate our own animations on it
    /// yet; it is here because `org.gnome.Shell.Introspect` publishes it and the portal reads it
    /// to decide whether to animate its dialogs (`introspect.js:184-192`).
    pub enable_animations: bool,
    /// `org.gnome.desktop.interface enable-hot-corners`: whether the top-left corner toggles the
    /// overview when the pointer pushes into it (`layout.js:436-443`). GNOME has exactly one hot
    /// corner and no way to move it; which corner is a text-direction question, not a preference.
    pub enable_hot_corners: bool,
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
    /// `org.gnome.desktop.app-folders`: the user's app-grid folders, in
    /// `folder-children` order. See [`AppFolder`].
    pub app_folders: Vec<AppFolder>,
    /// The accessibility state behind the `a11y` panel indicator and its menu.
    pub a11y: A11ySettings,
    /// `org.gnome.desktop.peripherals.*`: the pointer and keyboard device settings, which
    /// GNOME's way replaces niri's `input {}` block with. See [`Peripherals`].
    pub peripherals: Peripherals,
}

/// One row of the accessibility menu (`js/ui/status/accessibility.js:45-81`), in
/// gnome-shell's construction order — which is the order the menu shows them in.
///
/// Every row is a `PopupSwitchMenuItem`; nine are a plain boolean key, and
/// [`A11yToggle::LargeText`] is the odd one out (see [`A11ySettings::large_text`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum A11yToggle {
    HighContrast,
    Zoom,
    LargeText,
    ScreenReader,
    ScreenKeyboard,
    VisualAlerts,
    StickyKeys,
    SlowKeys,
    BounceKeys,
    MouseKeys,
}

impl A11yToggle {
    /// Every row, in gnome-shell's menu order (`accessibility.js:45-81`).
    pub const ALL: [A11yToggle; 10] = [
        A11yToggle::HighContrast,
        A11yToggle::Zoom,
        A11yToggle::LargeText,
        A11yToggle::ScreenReader,
        A11yToggle::ScreenKeyboard,
        A11yToggle::VisualAlerts,
        A11yToggle::StickyKeys,
        A11yToggle::SlowKeys,
        A11yToggle::BounceKeys,
        A11yToggle::MouseKeys,
    ];

    /// The row label (`accessibility.js:45-81`).
    pub fn label(self) -> &'static str {
        match self {
            A11yToggle::HighContrast => "High Contrast",
            A11yToggle::Zoom => "Zoom",
            A11yToggle::LargeText => "Large Text",
            A11yToggle::ScreenReader => "Screen Reader",
            A11yToggle::ScreenKeyboard => "Screen Keyboard",
            A11yToggle::VisualAlerts => "Visual Alerts",
            A11yToggle::StickyKeys => "Sticky Keys",
            A11yToggle::SlowKeys => "Slow Keys",
            A11yToggle::BounceKeys => "Bounce Keys",
            A11yToggle::MouseKeys => "Mouse Keys",
        }
    }

    /// The `(store, key)` this row is a plain boolean mirror of, or `None` for
    /// [`A11yToggle::LargeText`], whose key is a scaling *factor*.
    fn bool_key(self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            A11yToggle::HighContrast => ("a11y-interface", "high-contrast"),
            A11yToggle::Zoom => ("a11y-applications", "screen-magnifier-enabled"),
            A11yToggle::LargeText => return None,
            A11yToggle::ScreenReader => ("a11y-applications", "screen-reader-enabled"),
            A11yToggle::ScreenKeyboard => ("a11y-applications", "screen-keyboard-enabled"),
            A11yToggle::VisualAlerts => ("wm-preferences", "visual-bell"),
            A11yToggle::StickyKeys => ("a11y-keyboard", "stickykeys-enable"),
            A11yToggle::SlowKeys => ("a11y-keyboard", "slowkeys-enable"),
            A11yToggle::BounceKeys => ("a11y-keyboard", "bouncekeys-enable"),
            A11yToggle::MouseKeys => ("a11y-keyboard", "mousekeys-enable"),
        })
    }
}

/// The accessibility keys gnome-shell's `ATIndicator` reads and writes
/// (`js/ui/status/accessibility.js`).
///
/// We mirror the keys faithfully, but almost none of them have a consumer in this
/// stack yet: the magnifier, the on-screen keyboard, the keyboard filters, the visual
/// bell and the screen reader are separate subsystems, and our own chrome does not
/// follow `high-contrast`/`text-scaling-factor` the way St does. Writing the canonical
/// key is still the right port — GTK apps read `high-contrast` and
/// `text-scaling-factor` directly, and each consumer is its own later slice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct A11ySettings {
    /// `org.gnome.desktop.a11y always-show-universal-access-status`: pin the panel
    /// indicator on even with everything off (`accessibility.js:93-96`).
    pub always_show: bool,
    /// The nine plain-boolean rows, indexed by [`A11yToggle::ALL`] position (the
    /// `LargeText` slot is unused — its state lives in `text_scaling_factor`).
    bools: [bool; 10],
    /// `org.gnome.desktop.interface text-scaling-factor` (`d`). The Large Text row is
    /// on iff this is `> 1.0` (`accessibility.js:120-122`); turning it on writes
    /// `DPI_FACTOR_LARGE` = 1.25 and turning it off **resets** the key rather than
    /// writing 1.0 (`accessibility.js:124-129`).
    pub text_scaling_factor: f64,
}

/// `DPI_FACTOR_LARGE` (`accessibility.js:21`) — what the Large Text row writes.
pub const DPI_FACTOR_LARGE: f64 = 1.25;

impl Default for A11ySettings {
    fn default() -> Self {
        Self {
            always_show: false,
            bools: [false; 10],
            // The schema default; anything at or below 1.0 reads as Large Text off.
            text_scaling_factor: 1.0,
        }
    }
}

impl A11ySettings {
    /// Whether the Large Text row reads as on (`accessibility.js:122`).
    pub fn large_text(&self) -> bool {
        self.text_scaling_factor > 1.0
    }

    /// Whether `toggle` is on.
    pub fn get(&self, toggle: A11yToggle) -> bool {
        match toggle {
            A11yToggle::LargeText => self.large_text(),
            other => self.bools[Self::index(other)],
        }
    }

    /// Set `toggle` in the model. For [`A11yToggle::LargeText`] this moves the
    /// factor the same way a write would, so the optimistic update and the value
    /// that comes back from the store agree.
    pub fn set(&mut self, toggle: A11yToggle, on: bool) {
        match toggle {
            A11yToggle::LargeText => {
                self.text_scaling_factor = if on { DPI_FACTOR_LARGE } else { 1.0 };
            }
            other => self.bools[Self::index(other)] = on,
        }
    }

    /// Whether any row is on — half of the indicator's visibility predicate
    /// (`_syncMenuVisibility`, `accessibility.js:96`).
    pub fn any_active(&self) -> bool {
        A11yToggle::ALL.iter().any(|&t| self.get(t))
    }

    /// Whether the panel indicator is shown: `alwaysShow || items.some(f => !!f.state)`
    /// (`accessibility.js:96`).
    pub fn indicator_visible(&self) -> bool {
        self.always_show || self.any_active()
    }

    fn index(toggle: A11yToggle) -> usize {
        A11yToggle::ALL.iter().position(|&t| t == toggle).unwrap()
    }
}

/// One app-grid folder, as `FolderIcon`/`FolderView` read it
/// (`js/ui/appDisplay.js`).
///
/// Ids come from `org.gnome.desktop.app-folders folder-children`; each has its own
/// *relocatable* `org.gnome.desktop.app-folders.folder` instance at
/// `/org/gnome/desktop/app-folders/folders/<id>/` (`appDisplay.js:1510-1513`).
/// Resolving this to a member list is
/// [`AppSystem::folder_members`](crate::app_system::AppSystem::folder_members).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppFolder {
    /// The `folder-children` id, e.g. `"Utilities"`. Folder ids share the
    /// `app-picker-layout` id space with desktop ids, so a folder sorts into the
    /// grid exactly like an app does.
    pub id: String,
    /// The displayed name, already resolved through the `.directory` translation
    /// when the folder's `translate` key asked for it (`_getFolderName`,
    /// `appDisplay.js:97-104`).
    pub name: String,
    /// `categories`: the folder claims every shown app whose `Categories`
    /// intersect this list.
    pub categories: Vec<String>,
    /// `apps`: explicitly-placed members, in order — they come first.
    pub apps: Vec<String>,
    /// `excluded-apps`: ids the category match must not pull in.
    pub excluded_apps: Vec<String>,
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
            switchers: SwitcherSettings::default(),
            disable_command_line: false,
            shield: Default::default(),
            focus_new_windows: FocusNewWindows::Smart,
            edge_tiling: true,
            enable_animations: true,
            enable_hot_corners: true,
            background: BackgroundSettings::default(),
            accent_color: ACCENT_BLUE,
            app_picker_layout: HashMap::new(),
            base_font_pt: crate::ui::BASE_FONT_PT,
            base_font_family: synoik_vk::text::DEFAULT_SANS_FAMILY.to_owned(),
            key_theme: crate::ui::text_edit::KeyTheme::default(),
            icon_theme: "Adwaita".to_string(),
            clock: ClockFormat::default(),
            calendar: CalendarSettings::default(),
            quick_toggles: QuickToggles::default(),
            last_power_profile: "power-saver".to_string(),
            input_sources: InputSources::default(),
            world_clocks: WorldClocks::default(),
            app_folders: Vec::new(),
            a11y: A11ySettings::default(),
            peripherals: Peripherals::default(),
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
        if settings_has_key(shell, "app-picker-layout") {
            self.app_picker_layout = read_app_picker_layout(&shell.value("app-picker-layout"));
        }
    }

    /// The two switcher schemas (`data/org.gnome.shell.gschema.xml.in:307-343`).
    ///
    /// Each key is only read when the schema actually carries it, like every other loader here —
    /// a system with an older gnome-shell installed keeps our defaults rather than panicking in
    /// `gio`.
    fn load_switchers(&mut self, app: Option<&gio::Settings>, window: Option<&gio::Settings>) {
        if let Some(app) = app {
            if settings_has_key(app, "current-workspace-only") {
                self.switchers.apps_current_workspace_only = app.boolean("current-workspace-only");
            }
        }
        if let Some(window) = window {
            if settings_has_key(window, "current-workspace-only") {
                self.switchers.windows_current_workspace_only =
                    window.boolean("current-workspace-only");
            }
        }
    }

    fn load_lockdown(&mut self, lockdown: &gio::Settings) {
        if settings_has_key(lockdown, "disable-command-line") {
            self.disable_command_line = lockdown.boolean("disable-command-line");
        }
        if settings_has_key(lockdown, "disable-lock-screen") {
            self.shield.disable_lock_screen = lockdown.boolean("disable-lock-screen");
        }
        if settings_has_key(lockdown, "disable-show-password") {
            self.shield.disable_show_password = lockdown.boolean("disable-show-password");
        }
        if settings_has_key(lockdown, "disable-user-switching") {
            self.shield.disable_user_switching = lockdown.boolean("disable-user-switching");
        }
    }

    /// `org.gnome.login-screen` — which authentication services the shell is allowed to offer
    /// (`util.js:32-35`).
    ///
    /// Only whether to *look* for a reader; a machine with the key on and no hardware still shows
    /// nothing, because everything downstream gates on a reader having been found.
    fn load_login_screen(&mut self, login_screen: &gio::Settings) {
        if settings_has_key(login_screen, "enable-fingerprint-authentication") {
            self.shield.enable_fingerprint =
                login_screen.boolean("enable-fingerprint-authentication");
        }
        if settings_has_key(login_screen, "enable-smartcard-authentication") {
            self.shield.enable_smartcard = login_screen.boolean("enable-smartcard-authentication");
        }
    }

    fn load_screensaver(&mut self, screensaver: &gio::Settings) {
        if settings_has_key(screensaver, "lock-enabled") {
            self.shield.lock_enabled = screensaver.boolean("lock-enabled");
        }
        if settings_has_key(screensaver, "lock-delay") {
            self.shield.lock_delay =
                std::time::Duration::from_secs(u64::from(screensaver.uint("lock-delay")));
        }
        if settings_has_key(screensaver, "user-switch-enabled") {
            self.shield.user_switch_enabled = screensaver.boolean("user-switch-enabled");
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
        if settings_has_key(interface, "enable-animations") {
            self.enable_animations = interface.boolean("enable-animations");
        }
        if settings_has_key(interface, "enable-hot-corners") {
            self.enable_hot_corners = interface.boolean("enable-hot-corners");
        }
        if settings_has_key(interface, "accent-color") {
            let value = interface.string("accent-color");
            match parse_accent_color(value.as_str()) {
                Some(rgb) => self.accent_color = rgb,
                None => warn!("ignoring unrecognized accent-color {value:?}"),
            }
        }
        if settings_has_key(interface, "gtk-key-theme") {
            self.key_theme = crate::ui::text_edit::KeyTheme::from_setting(
                interface.string("gtk-key-theme").as_str(),
            );
        }
        if settings_has_key(interface, "icon-theme") {
            let value = interface.string("icon-theme");
            if !value.is_empty() {
                self.icon_theme = value.to_string();
            }
        }
        if settings_has_key(interface, "font-name") {
            let value = interface.string("font-name");
            match parse_font_size_pt(value.as_str()) {
                Some(pt) => self.base_font_pt = pt,
                None => warn!("ignoring font-name {value:?} with no point size"),
            }
            match parse_font_family(value.as_str()) {
                Some(family) => self.base_font_family = family,
                None => warn!("ignoring font-name {value:?} with no family"),
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

    /// Read the accessibility state (`ATIndicator`, `js/ui/status/accessibility.js`).
    /// The rows live across five schemas, so this takes the whole [`Stores`] rather
    /// than one store; each is skipped where the schema (or the key) is absent.
    fn load_a11y(&mut self, stores: &Stores) {
        if let Some(a11y) = &stores.a11y {
            if settings_has_key(a11y, "always-show-universal-access-status") {
                self.a11y.always_show = a11y.boolean("always-show-universal-access-status");
            }
        }
        for toggle in A11yToggle::ALL {
            let Some((store, key)) = toggle.bool_key() else {
                continue;
            };
            if let Some(settings) = stores.get(store) {
                if settings_has_key(settings, key) {
                    self.a11y.set(toggle, settings.boolean(key));
                }
            }
        }
        // Large Text is a factor, not a flag (`accessibility.js:118-129`).
        if let Some(interface) = &stores.interface {
            if settings_has_key(interface, "text-scaling-factor") {
                self.a11y.text_scaling_factor = interface.double("text-scaling-factor");
            }
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
        shell_keybindings: Option<&gio::Settings>,
        wayland_keybindings: Option<&gio::Settings>,
        synoik_keybindings: Option<&gio::Settings>,
    ) {
        let mut keybindings = read_keybinding_table(wm, adopted_wm_keybindings());
        keybindings.extend(read_keybinding_table(
            mutter_keybindings,
            adopted_mutter_keybindings(),
        ));
        keybindings.extend(read_keybinding_table(
            shell_keybindings,
            adopted_shell_keybindings(),
        ));
        keybindings.extend(read_keybinding_table(
            wayland_keybindings,
            adopted_wayland_keybindings(),
        ));
        // Ours last: GNOME's bindings are matched first, so a chord in both models
        // resolves to GNOME's. The collision test means that should never happen,
        // but the order is the belt to its braces.
        keybindings.extend(read_keybinding_table(
            synoik_keybindings,
            adopted_synoik_keybindings(),
        ));
        self.keybindings = keybindings;
    }
}

/// Reads one adopted-keybindings table, one entry per settings key in table
/// order, from its store where open (a missing key falls back to our
/// built-in default rather than aborting inside gio).
/// One row of an adopted-keybindings table: the settings key, what it does, the
/// default accelerators, and optionally a cooldown.
///
/// A trait rather than a four-field tuple everywhere, so the GNOME tables — none
/// of which has a cooldown — keep their three-element rows.
trait AdoptedKey {
    fn into_parts(self) -> (String, KeybindingAction, Vec<String>, Option<Duration>);
}

impl<A: Into<KeybindingAction>> AdoptedKey for (String, A, Vec<String>) {
    fn into_parts(self) -> (String, KeybindingAction, Vec<String>, Option<Duration>) {
        (self.0, self.1.into(), self.2, None)
    }
}

impl<A: Into<KeybindingAction>> AdoptedKey for (String, A, Vec<String>, Option<Duration>) {
    fn into_parts(self) -> (String, KeybindingAction, Vec<String>, Option<Duration>) {
        (self.0, self.1.into(), self.2, self.3)
    }
}

fn read_keybinding_table<K: AdoptedKey>(
    store: Option<&gio::Settings>,
    table: Vec<K>,
) -> Vec<GnomeKeybinding> {
    table
        .into_iter()
        .map(AdoptedKey::into_parts)
        .map(|(key, action, defaults, cooldown)| {
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
                cooldown,
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
    pub action: KeybindingAction,
    pub accels: Vec<Accel>,
    /// How long after firing before this binding may fire again — niri's
    /// `cooldown-ms`, and `None` for everything GNOME names.
    ///
    /// It exists for the scroll bindings: a wheel detent is not a keypress, and a
    /// free-spinning wheel would otherwise walk a dozen workspaces per flick.
    pub cooldown: Option<Duration>,
}

/// What a keybinding does: one of GNOME's semantic actions, or — for the
/// scrolling-window-manager behaviors GNOME has no equivalent for — a niri
/// action straight from our own schema.
///
/// The niri arm carries [`Action`] rather than growing [`GnomeKeyAction`] into a
/// mirror of it. The tables stay hand-curated either way (the `.gschema.xml` has
/// to enumerate its keys), and keeping niri's actions in niri's vocabulary makes
/// it obvious which half of the model a binding belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum KeybindingAction {
    Gnome(GnomeKeyAction),
    Synoik(Action),
}

impl From<GnomeKeyAction> for KeybindingAction {
    fn from(action: GnomeKeyAction) -> Self {
        Self::Gnome(action)
    }
}

impl From<Action> for KeybindingAction {
    fn from(action: Action) -> Self {
        Self::Synoik(action)
    }
}

impl KeybindingAction {
    /// The GNOME action, for the callers that only speak that half — the
    /// switcher's modal allowlist and the NON_MASKABLE check.
    pub fn gnome(&self) -> Option<GnomeKeyAction> {
        match self {
            Self::Gnome(action) => Some(*action),
            Self::Synoik(_) => None,
        }
    }
}

/// The semantic actions of the adopted GNOME keybindings, named after their
/// `org.gnome.desktop.wm.keybindings` keys. The four directional workspace
/// keys collapse to previous/next: GNOME's workspaces are one linear row
/// (left/right; up/down are the pre-GNOME-40 vertical-layout legacy), and both
/// axes map onto niri's vertical workspace column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GnomeKeyAction {
    /// `show-screenshot-ui`: the shell's screenshot picker (`<Shift>Print`).
    ShowScreenshotUi,
    /// `screenshot`: capture the whole screen straight to a file, no UI (`Print`).
    Screenshot,
    /// `screenshot-window`: the focused window straight to a file (`<Alt>Print`).
    ScreenshotWindow,
    /// `show-screen-recording-ui`: `<Ctrl><Shift><Alt>R`.
    ///
    /// In GNOME this *opens the screenshot UI in recording mode* rather than recording anything.
    /// Until that UI is ported it starts and stops a recording directly — see the divergence note
    /// on [`adopted_shell_keybindings`].
    ShowScreenRecordingUi,
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
    /// `switch-group` / `switch-group-backward`: the app switcher again, opened
    /// *within* the current app — the window sub-list comes up on item 0
    /// (`altTab.js:117-137`). Its default accel is `Above_Tab`, the key
    /// physically above Tab.
    SwitchGroup { backward: bool },
    /// `cycle-windows` / `cycle-windows-backward`: `WindowCyclerPopup`
    /// (`altTab.js:638-667`) — the same window list as `switch-windows` with
    /// no popup at all; the selection is shown by raising the window and
    /// framing it with `.cycler-highlight`.
    CycleWindows { backward: bool },
    /// `cycle-group` / `cycle-group-backward`: `GroupCyclerPopup`
    /// (`altTab.js:541-580`), the same listless cycler over the focused app's
    /// windows only.
    CycleGroup { backward: bool },
    /// `switch-applications` / `switch-applications-backward`: GNOME's
    /// Super-Tab — `AppSwitcherPopup`, one item per application, spanning
    /// workspaces unless `org.gnome.shell.app-switcher current-workspace-only`
    /// says otherwise (`docs/fork/alt-tab-port.md`).
    ///
    /// This carried an "accepted divergence: no app grouping, mapped onto the
    /// window MRU switcher" note long after the switcher port retired `mru.rs`
    /// and made it false.
    SwitchApplications { backward: bool },
    /// `maximize`: maximize the focused window.
    Maximize,
    /// `unmaximize`: unmaximize the focused window; a tiled window untiles.
    Unmaximize,
    /// `toggle-tiled-left` / `-right` (`org.gnome.mutter.keybindings`): tile
    /// the focused window to the given half of the work area, or untile it if
    /// already tiled there.
    ToggleTiled(TileSide),
    /// `toggle-maximized` (`<Alt>F10`): maximize the focused window, or
    /// unmaximize it if it already is.
    ToggleMaximized,
    /// `switch-to-workspace-last` (`<Super>End`).
    SwitchToWorkspaceLast,
    /// `move-to-workspace-last` (`<Super><Shift>End`).
    MoveToWorkspaceLast,
    /// `move-to-monitor-{left,right,up,down}` (`<Super><Shift>` + arrows): move
    /// the focused window to the neighbouring monitor.
    MoveToMonitor(ScreenDirection),
    /// `switch-input-source` / `-backward` (`<Super>space`): step through the
    /// configured keyboard layouts.
    ///
    /// **DIVERGENCE:** gnome-shell puts up an input-source switcher popup for
    /// the duration of the modifier hold; we switch straight away. The popup is
    /// the same shape as the alt-tab switchers and belongs with them.
    SwitchInputSource { backward: bool },
    /// `switch-to-application-N` (`<Super>1..9`, 1-based): activate the Nth dash
    /// favourite.
    SwitchToApplication(u8),
    /// `open-new-window-application-N` (`<Super><Ctrl>1..9`, 1-based).
    OpenNewWindowApplication(u8),
    /// `toggle-overview` (`org.gnome.shell.keybindings`): the overview. Unbound by
    /// default — GNOME opens the overview with the overlay key.
    ToggleOverview,
    /// `toggle-application-view` (`<Super>a`): the app grid.
    ToggleApplicationView,
    /// `toggle-message-tray` (`<Super>v` / `<Super>m`): the date menu — calendar
    /// and message list.
    ToggleMessageTray,
    /// `toggle-quick-settings` (`<Super>s`): the quick settings menu.
    ToggleQuickSettings,
    /// `restore-shortcuts` (`org.gnome.mutter.wayland.keybindings`, `<Super>Escape`):
    /// hand the shortcuts back to the compositor while a client is inhibiting them.
    RestoreShortcuts,
    /// `switch-to-session-N` (`org.gnome.mutter.wayland.keybindings`, 1-based): change
    /// to VT N. Registered by mutter only on the native backend
    /// (`NATIVE_KEYBINDINGS`, `keybindings.c`).
    SwitchToSession(u8),
    /// `screen-brightness-{up,down,cycle}[-monitor]` (`org.gnome.shell.keybindings`): step the
    /// brightness scales. The `-monitor` variants act on the monitor under the pointer, which is
    /// gnome-shell's `get_current_logical_monitor()` (`brightnessManager.js:107-132`).
    ScreenBrightness {
        step: crate::brightness::Step,
        current_monitor: bool,
    },
}

impl GnomeKeyAction {
    /// Whether this binding survives a client's keyboard-shortcuts inhibitor —
    /// mutter's `META_KEY_BINDING_NON_MASKABLE`, which `process_event` checks
    /// before consulting `meta_window_shortcuts_inhibited`.
    ///
    /// Only the recovery keys carry it, and for a reason worth stating: an
    /// inhibiting client that could also swallow these would leave the user with
    /// no way to take the keyboard back or to change VT. Deriving it from the
    /// action rather than storing it per keybinding keeps it from drifting out
    /// of sync with what the action actually does.
    pub(crate) fn is_non_maskable(self) -> bool {
        matches!(self, Self::RestoreShortcuts | Self::SwitchToSession(_))
    }
}

/// One of the four screen directions, for the keys that act on a neighbour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenDirection {
    Left,
    Right,
    Up,
    Down,
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

/// The `org.gnome.desktop.wm.keybindings` keys we honor, with the default
/// accelerators we ship for each.
///
/// These are the *session's* defaults, not a transcription of the upstream
/// schema: where we deliberately differ we say so at the key, and the same
/// values belong in the `.gschema.override` we install (upstream owns that
/// schema — it comes from gsettings-desktop-schemas, which survives replacing
/// mutter and gnome-shell). Anywhere we do *not* mean to differ, the value must
/// match the schema verbatim: these apply where the schema isn't installed,
/// which includes the whole test corpus, so an accidental invention hides on a
/// GNOME box while quietly defining what the conformance tests assert.
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
        // DIVERGENCE (deliberate, ours): upstream leaves `switch-windows` empty and
        // gives `<Alt>Tab` to `switch-applications`, so a stock GNOME Alt+Tab is the
        // *application* switcher. We ship Alt+Tab as the *window* switcher and leave
        // Super+Tab to the applications. These are the defaults we intend to install
        // as a `.gschema.override`, so this table is our shipped default rather than a
        // transcription of the upstream schema — see docs/fork/keybindings-port.md.
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
            "switch-group".to_owned(),
            SwitchGroup { backward: false },
            strs(&["<Super>Above_Tab", "<Alt>Above_Tab"]),
        ),
        (
            "switch-group-backward".to_owned(),
            SwitchGroup { backward: true },
            strs(&["<Shift><Super>Above_Tab", "<Shift><Alt>Above_Tab"]),
        ),
        (
            "cycle-windows".to_owned(),
            CycleWindows { backward: false },
            strs(&["<Alt>Escape"]),
        ),
        (
            "cycle-windows-backward".to_owned(),
            CycleWindows { backward: true },
            strs(&["<Shift><Alt>Escape"]),
        ),
        (
            "cycle-group".to_owned(),
            CycleGroup { backward: false },
            strs(&["<Alt>F6"]),
        ),
        (
            "cycle-group-backward".to_owned(),
            CycleGroup { backward: true },
            strs(&["<Shift><Alt>F6"]),
        ),
        (
            "toggle-maximized".to_owned(),
            ToggleMaximized,
            strs(&["<Alt>F10"]),
        ),
        (
            "switch-to-workspace-last".to_owned(),
            SwitchToWorkspaceLast,
            strs(&["<Super>End"]),
        ),
        (
            "move-to-workspace-last".to_owned(),
            MoveToWorkspaceLast,
            strs(&["<Super><Shift>End"]),
        ),
        (
            "move-to-monitor-left".to_owned(),
            MoveToMonitor(ScreenDirection::Left),
            strs(&["<Super><Shift>Left"]),
        ),
        (
            "move-to-monitor-right".to_owned(),
            MoveToMonitor(ScreenDirection::Right),
            strs(&["<Super><Shift>Right"]),
        ),
        (
            "move-to-monitor-up".to_owned(),
            MoveToMonitor(ScreenDirection::Up),
            strs(&["<Super><Shift>Up"]),
        ),
        (
            "move-to-monitor-down".to_owned(),
            MoveToMonitor(ScreenDirection::Down),
            strs(&["<Super><Shift>Down"]),
        ),
        // Owned by gnome-shell's input-source manager rather than any mutter table,
        // but it is a key in this schema.
        (
            "switch-input-source".to_owned(),
            SwitchInputSource { backward: false },
            strs(&["<Super>space", "XF86Keyboard"]),
        ),
        (
            "switch-input-source-backward".to_owned(),
            SwitchInputSource { backward: true },
            strs(&["<Shift><Super>space", "<Shift>XF86Keyboard"]),
        ),
        // Super+Tab only — upstream also lists `<Alt>Tab` here; see the divergence note
        // on `switch-windows` above.
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
        // Only workspace 1 is bound out of the box, to `<Super>Home`. `<Super>N` is *not*
        // workspace N in GNOME — it is `switch-to-application-N` from
        // `org.gnome.shell.keybindings` (gnome-shell's schema, `switch-to-application-1`
        // = `<Super>1`), i.e. the Nth dash favorite.
        let defaults = if n == 1 {
            strs(&["<Super>Home"])
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
    keybindings.extend(read_keybinding_table(None, adopted_shell_keybindings()));
    keybindings.extend(read_keybinding_table(None, adopted_wayland_keybindings()));
    keybindings.extend(read_keybinding_table(None, adopted_synoik_keybindings()));
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
    load_and_watch_gsettings_with(SettingsStore::System)
}

/// [`load_and_watch_gsettings`] against a chosen [`SettingsStore`], so a test can run this
/// exact watcher — real thread, real subscription, real dedup, real writer — on a private
/// in-memory store.
pub fn load_and_watch_gsettings_with(
    store: SettingsStore,
) -> (
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
                let stores = Rc::new(Stores::open(store));
                STORES.set(Some(stores.clone()));

                // Subscribe before the initial read so no change can fall
                // between them; a racing change just re-arrives via `tx`.
                //
                // A change to *any* key in *any* watched store re-reads the whole
                // model, and most of those keys are not ones we model — so the usual
                // ping produces a model identical to the one already applied. Sending
                // it anyway made the main loop re-derive everything downstream for
                // nothing. Emit only on a real difference; the consumer then knows a
                // delivered model genuinely changed.
                let last: Rc<RefCell<Option<GnomeSettings>>> = Rc::new(RefCell::new(None));
                let seen = last.clone();
                stores.subscribe(move |settings| {
                    if !is_new_model(&seen, &settings) {
                        return;
                    }
                    let _ = tx.send(settings);
                });
                // Mirror GNOME Clocks' locations into the shell gsettings before the
                // initial read, so a running Clocks is reflected in the first model.
                stores.setup_clocks_mirror();
                let initial = stores.read();
                // Seed the dedup with what the consumer starts from, so the first
                // unrelated ping after startup compares against it rather than
                // counting as a change. (A change *during* startup still gets through:
                // `last` is None until here, so those callbacks always send.)
                *last.borrow_mut() = Some(initial.clone());
                let _ = init_tx.send(initial);

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
    /// no-op, so the authoritative copy lives on `Synoik` and this is best-effort persistence.
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

    /// Persist the app-grid arrangement: `org.gnome.shell app-picker-layout`, an
    /// `aa{sv}` of one dict per page mapping each app id to a boxed
    /// `{'position': <int32>}` (`AppDisplay._savePages`, `appDisplay.js:1387-1404`).
    /// The read side is `read_app_picker_layout`. Missing store/key is a no-op.
    ///
    /// The write re-enters our own `changed` subscription, which re-sorts the grid from
    /// the new key — that is why it has to describe the order we are already showing.
    pub fn set_app_picker_layout(&self, pages: Vec<Vec<String>>) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(shell) = &s.shell {
                    if settings_has_key(shell, "app-picker-layout") {
                        let value = build_app_picker_layout(&pages);
                        if let Err(err) = shell.set_value("app-picker-layout", &value) {
                            warn!("error writing org.gnome.shell app-picker-layout: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Seed the default app folders on a profile that has never had any —
    /// `AppDisplay._ensureDefaultFolders` (`appDisplay.js:1406-1431`), which
    /// gnome-shell runs once from `AppDisplay._init`.
    ///
    /// Without it a fresh profile shows **no** folders at all: `folder-children` starts
    /// empty and nothing else ever writes it, which is why the live seat had no
    /// Utilities folder while a profile that had run stock gnome-shell did.
    ///
    /// The guard is `folder-children` having *no user value* **and** reading empty. A
    /// user who deletes every folder leaves a user value behind, so the defaults do not
    /// come back — that is the whole point of checking both.
    ///
    /// `folders` is [`default_folders`] with each app list already filtered to what is
    /// installed (the caller owns the catalog); folders that filter down to nothing are
    /// still listed, because an empty folder is simply not displayed.
    pub fn ensure_default_folders(&self, folders: Vec<DefaultFolder>) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(app_folders) = &s.app_folders {
                    // Opened by `Stores::open` on the default backend, so the folder
                    // stores go there too.
                    ensure_default_folders(app_folders, &folders, None);
                }
                stores.set(Some(s));
            });
        });
    }

    /// Create a folder with id `id` holding `apps`, named `name`. The write hops onto
    /// the watcher thread like every other one, so the caller sees it through the usual
    /// reload — which is why the id is the caller's ([`new_folder_id`]): it has to place
    /// the folder in its own model long before the reload gets back.
    pub fn create_app_folder(&self, id: &str, name: String, apps: Vec<String>) {
        let (folder_id, name, apps) = (id.to_owned(), name, apps);
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(app_folders) = &s.app_folders {
                    create_app_folder(app_folders, &folder_id, &name, &apps, s.backend.as_ref());
                }
                stores.set(Some(s));
            });
        });
    }

    /// Add `app` to the folder `id` (`FolderView.addApp`, `appDisplay.js:2223-2236`).
    /// A read-modify-write on the watcher thread, because `apps` is not the folder's
    /// membership: a categories-based folder sweeps in members that were never listed
    /// there, and writing the *resolved* list back would freeze the sweep.
    pub fn add_to_app_folder(&self, id: &str, app: &str) {
        let (folder_id, app) = (id.to_owned(), app.to_owned());
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if s.app_folders.is_some() {
                    add_to_app_folder(&folder_id, &app, s.backend.as_ref());
                }
                stores.set(Some(s));
            });
        });
    }

    /// Take `app` out of the folder `id` (`FolderView.removeApp`, `appDisplay.js:2239-2272`).
    /// A read-modify-write for the same reason [`Self::add_to_app_folder`] is; a folder
    /// with `categories` also gets the app pushed onto `excluded-apps`, which is the only
    /// thing that keeps a swept-in app out.
    pub fn remove_from_app_folder(&self, id: &str, app: &str) {
        let (folder_id, app) = (id.to_owned(), app.to_owned());
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if s.app_folders.is_some() {
                    remove_from_app_folder(&folder_id, &app, s.backend.as_ref());
                }
                stores.set(Some(s));
            });
        });
    }

    /// Write the folder `id`'s members back in the order a drag left them
    /// (`FolderView.acceptDrop`, `appDisplay.js:2213-2221`).
    ///
    /// The one folder write that is *not* a read-modify-write, and GNOME's is not either:
    /// `_orderedItems` is the resolved membership, so for a categories-based folder this
    /// does list the swept-in apps in `apps` explicitly. `categories` stays, so the sweep
    /// still picks up anything installed later; only the apps that were already there
    /// gain an explicit position — which is the point of reordering them.
    pub fn set_app_folder_apps(&self, id: &str, apps: Vec<String>) {
        let folder_id = id.to_owned();
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if s.app_folders.is_some() {
                    if let Some(store) = folder_settings(&folder_id, s.backend.as_ref()) {
                        let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
                        if let Err(err) = store.set_strv("apps", refs) {
                            warn!("error reordering the app folder {folder_id}: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Rename the folder `id` (`_maybeUpdateFolderName`, `appDisplay.js:2650-2657`).
    /// `translate` goes off with it: a user-typed name is the string to show, not a
    /// `.directory` basename to look up.
    pub fn rename_app_folder(&self, id: &str, name: String) {
        let (folder_id, name) = (id.to_owned(), name);
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if s.app_folders.is_some() {
                    if let Some(store) = folder_settings(&folder_id, s.backend.as_ref()) {
                        if let Err(err) = store.set_string("name", &name) {
                            warn!("error renaming the app folder {folder_id}: {err}");
                        } else {
                            let _ = store.set_boolean("translate", false);
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Delete the folder `id` — what emptying it does (`FolderView.removeApp`'s
    /// `folderApps.length === 0` branch, `appDisplay.js:2245-2262`).
    pub fn delete_app_folder(&self, id: &str) {
        let folder_id = id.to_owned();
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(app_folders) = &s.app_folders {
                    delete_app_folder(app_folders, &folder_id, s.backend.as_ref());
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

    /// Flip one accessibility menu row (`ATIndicator._buildItem` /
    /// `_buildFontItem`, `js/ui/status/accessibility.js:107-146`).
    ///
    /// Nine rows are a plain boolean key. Large Text is not: it writes
    /// [`DPI_FACTOR_LARGE`] on, and **resets** `text-scaling-factor` off rather than
    /// writing 1.0 (`accessibility.js:126-128`) — the difference is visible to anyone
    /// reading the key's user value, and resetting is what lets a system default
    /// other than 1.0 come back.
    pub fn set_a11y_toggle(&self, toggle: A11yToggle, on: bool) {
        match toggle.bool_key() {
            Some((store, key)) => self.set_bool(store, key, on),
            None if on => self.set_double("interface", "text-scaling-factor", DPI_FACTOR_LARGE),
            None => self.reset_key("interface", "text-scaling-factor"),
        }
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

    /// Write a double key on one of the stores. Missing store/key is a no-op.
    fn set_double(&self, store: &'static str, key: &'static str, value: f64) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(settings) = s.get(store) {
                    if settings_has_key(settings, key) {
                        if let Err(err) = settings.set_double(key, value) {
                            warn!("error writing {store} {key}: {err}");
                        }
                    }
                }
                stores.set(Some(s));
            });
        });
    }

    /// Drop the user value of a key, letting the system default show through —
    /// `Gio.Settings.reset`. Missing store/key is a no-op.
    fn reset_key(&self, store: &'static str, key: &'static str) {
        self.ctx.invoke(move || {
            STORES.with(|stores| {
                let Some(s) = stores.take() else { return };
                if let Some(settings) = s.get(store) {
                    if settings_has_key(settings, key) {
                        settings.reset(key);
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
/// The `org.gnome.shell.keybindings` keys we honor. Unlike the wm/mutter tables these are the
/// *shell's* own bindings (`data/org.gnome.shell.gschema.xml.in:265-304`).
///
/// These replace niri's own screenshot/recording binds: the keys come from GNOME's settings, not
/// from the KDL config. `Action::ToggleScreenRecord` in particular had no default binding at all,
/// so there was no way to start a recording from the keyboard.
///
/// **Divergence, to retire with the screenshot UI port.** GNOME's `show-screen-recording-ui` opens
/// the screenshot UI switched to recording mode; the user then picks screen or area and presses a
/// button, and stops from the panel indicator. We have no such UI yet, so the key toggles a
/// full-screen recording directly. The binding is right, what it opens is not.
fn adopted_shell_keybindings() -> Vec<(String, GnomeKeyAction, Vec<String>)> {
    use crate::brightness::Step;

    fn entry(
        key: &str,
        step: Step,
        current_monitor: bool,
        accel: &str,
    ) -> (String, GnomeKeyAction, Vec<String>) {
        (
            key.to_owned(),
            GnomeKeyAction::ScreenBrightness {
                step,
                current_monitor,
            },
            vec![accel.to_owned()],
        )
    }

    let mut keys = vec![
        (
            "toggle-overview".to_owned(),
            GnomeKeyAction::ToggleOverview,
            Vec::new(),
        ),
        (
            "toggle-application-view".to_owned(),
            GnomeKeyAction::ToggleApplicationView,
            vec!["<Super>a".to_owned()],
        ),
        (
            "toggle-message-tray".to_owned(),
            GnomeKeyAction::ToggleMessageTray,
            vec!["<Super>v".to_owned(), "<Super>m".to_owned()],
        ),
        (
            "toggle-quick-settings".to_owned(),
            GnomeKeyAction::ToggleQuickSettings,
            vec!["<Super>s".to_owned()],
        ),
        // Print opens the picker; Shift+Print is the one that goes straight to a file.
        (
            "show-screenshot-ui".to_owned(),
            GnomeKeyAction::ShowScreenshotUi,
            vec!["Print".to_owned()],
        ),
        (
            "screenshot".to_owned(),
            GnomeKeyAction::Screenshot,
            vec!["<Shift>Print".to_owned()],
        ),
        (
            "screenshot-window".to_owned(),
            GnomeKeyAction::ScreenshotWindow,
            vec!["<Alt>Print".to_owned()],
        ),
        (
            "show-screen-recording-ui".to_owned(),
            GnomeKeyAction::ShowScreenRecordingUi,
            vec!["<Ctrl><Shift><Alt>R".to_owned()],
        ),
        entry(
            "screen-brightness-up",
            Step::Up,
            false,
            "XF86MonBrightnessUp",
        ),
        entry(
            "screen-brightness-up-monitor",
            Step::Up,
            true,
            "<Shift>XF86MonBrightnessUp",
        ),
        entry(
            "screen-brightness-down",
            Step::Down,
            false,
            "XF86MonBrightnessDown",
        ),
        entry(
            "screen-brightness-down-monitor",
            Step::Down,
            true,
            "<Shift>XF86MonBrightnessDown",
        ),
        entry(
            "screen-brightness-cycle",
            Step::Cycle,
            false,
            "XF86MonBrightnessCycle",
        ),
        entry(
            "screen-brightness-cycle-monitor",
            Step::Cycle,
            true,
            "<Shift>XF86MonBrightnessCycle",
        ),
    ];

    // The number row belongs to the dash, not to the workspaces: `<Super>N` activates
    // the Nth favourite and `<Super><Ctrl>N` asks it for another window.
    for n in 1..=9u8 {
        keys.push((
            format!("switch-to-application-{n}"),
            GnomeKeyAction::SwitchToApplication(n),
            vec![format!("<Super>{n}")],
        ));
        keys.push((
            format!("open-new-window-application-{n}"),
            GnomeKeyAction::OpenNewWindowApplication(n),
            vec![format!("<Super><Control>{n}")],
        ));
    }

    keys
}

/// An accelerator's modifiers as the config's [`Modifiers`], following the same
/// equivalences [`accel_mods_match`] applies: the virtual META/HYPER masks live
/// on Alt and Super, and MOD4 is Super.
pub(crate) fn modifiers_from_accel(accel_mods: AccelMods) -> Modifiers {
    let mut mods = Modifiers::empty();
    if accel_mods.contains(AccelMods::CONTROL) {
        mods |= Modifiers::CTRL;
    }
    if accel_mods.contains(AccelMods::SHIFT) {
        mods |= Modifiers::SHIFT;
    }
    if accel_mods.intersects(AccelMods::MOD1 | AccelMods::META) {
        mods |= Modifiers::ALT;
    }
    if accel_mods.intersects(AccelMods::SUPER | AccelMods::HYPER | AccelMods::MOD4) {
        mods |= Modifiers::SUPER;
    }
    if accel_mods.contains(AccelMods::MOD5) {
        mods |= Modifiers::ISO_LEVEL3_SHIFT;
    }
    mods
}

/// An accelerator as a config [`Key`], for the surfaces that render bindings —
/// the hotkey overlay, which does not care which model a binding came from.
///
/// `None` for a raw-keycode accelerator (`0x29`, `Above_Tab`): there is no
/// layout-independent name to show for one.
pub(crate) fn key_for_accel(accel: &Accel) -> Option<synoik_config::Key> {
    let trigger = match accel.trigger {
        AccelTrigger::Keysym(keysym) => synoik_config::Trigger::Keysym(keysym),
        AccelTrigger::Device(trigger) => trigger,
        AccelTrigger::Keycode(_) => return None,
    };
    Some(synoik_config::Key {
        trigger,
        modifiers: modifiers_from_accel(accel.mods),
    })
}

/// Our own schema, `org.synoik.keybindings`: the scrolling-window-manager
/// behaviors GNOME has no equivalent for.
///
/// Mirrors `resources/schemas/org.synoik.keybindings.gschema.xml` key for key —
/// `our_schema_matches_the_table` fails if the two drift apart. Nothing here may
/// take a chord we adopt from GNOME; `synoik_accels_do_not_collide_with_gnome`
/// checks that, so the fork tenet is enforced rather than remembered.
///
/// Arrow keys are absent throughout: `<Super>` plus an arrow is GNOME's, four
/// times over (tiling, maximize, unmaximize, move-to-monitor), so this half of
/// the model is hjkl.
fn adopted_synoik_keybindings() -> Vec<(String, Action, Vec<String>, Option<Duration>)> {
    use Action::*;

    fn key(name: &str, action: Action, accel: &str) -> (String, Action, Vec<String>) {
        (name.to_owned(), action, vec![accel.to_owned()])
    }

    let keys = vec![
        key("focus-column-left", FocusColumnLeft, "<Super><Alt>h"),
        key("focus-column-right", FocusColumnRight, "<Super><Alt>l"),
        key("focus-window-up", FocusWindowUp, "<Super><Alt>k"),
        key("focus-window-down", FocusWindowDown, "<Super><Alt>j"),
        key(
            "focus-column-first",
            FocusColumnFirst,
            "<Super><Control>Home",
        ),
        key("focus-column-last", FocusColumnLast, "<Super><Control>End"),
        key("move-column-left", MoveColumnLeft, "<Super><Control>h"),
        key("move-column-right", MoveColumnRight, "<Super><Control>l"),
        key("move-window-up", MoveWindowUp, "<Super><Control>k"),
        key("move-window-down", MoveWindowDown, "<Super><Control>j"),
        key(
            "move-column-to-first",
            MoveColumnToFirst,
            "<Super><Control><Shift>Home",
        ),
        key(
            "move-column-to-last",
            MoveColumnToLast,
            "<Super><Control><Shift>End",
        ),
        key("focus-monitor-left", FocusMonitorLeft, "<Super><Shift>h"),
        key("focus-monitor-right", FocusMonitorRight, "<Super><Shift>l"),
        key("focus-monitor-up", FocusMonitorUp, "<Super><Shift>k"),
        key("focus-monitor-down", FocusMonitorDown, "<Super><Shift>j"),
        key(
            "move-column-to-monitor-left",
            MoveColumnToMonitorLeft,
            "<Super><Control><Shift>h",
        ),
        key(
            "move-column-to-monitor-right",
            MoveColumnToMonitorRight,
            "<Super><Control><Shift>l",
        ),
        key(
            "move-column-to-monitor-up",
            MoveColumnToMonitorUp,
            "<Super><Control><Shift>k",
        ),
        key(
            "move-column-to-monitor-down",
            MoveColumnToMonitorDown,
            "<Super><Control><Shift>j",
        ),
        key(
            "move-column-to-workspace-up",
            MoveColumnToWorkspaceUp(true),
            "<Super><Control>i",
        ),
        key(
            "move-column-to-workspace-down",
            MoveColumnToWorkspaceDown(true),
            "<Super><Control>u",
        ),
        key("move-workspace-up", MoveWorkspaceUp, "<Super><Shift>i"),
        key("move-workspace-down", MoveWorkspaceDown, "<Super><Shift>u"),
        key(
            "consume-or-expel-window-left",
            ConsumeOrExpelWindowLeft,
            "<Super>bracketleft",
        ),
        key(
            "consume-or-expel-window-right",
            ConsumeOrExpelWindowRight,
            "<Super>bracketright",
        ),
        key(
            "consume-window-into-column",
            ConsumeWindowIntoColumn,
            "<Super>comma",
        ),
        key(
            "expel-window-from-column",
            ExpelWindowFromColumn,
            "<Super>period",
        ),
        key(
            "switch-preset-column-width",
            SwitchPresetColumnWidth,
            "<Super>r",
        ),
        key(
            "switch-preset-column-width-back",
            SwitchPresetColumnWidthBack,
            "<Super><Shift>r",
        ),
        key(
            "switch-preset-window-height",
            SwitchPresetWindowHeight,
            "<Super><Control><Shift>r",
        ),
        key(
            "reset-window-height",
            ResetWindowHeight,
            "<Super><Control>r",
        ),
        // The step is compiled in rather than a settings key: mutter's keybinding
        // schema is accelerators only, and an `as` key has nowhere to put the amount.
        // Arbitrary sizes stay available over IPC (`synoik msg action set-column-width`).
        key(
            "grow-column-width",
            SetColumnWidth(SizeChange::AdjustProportion(10.)),
            "<Super>equal",
        ),
        key(
            "shrink-column-width",
            SetColumnWidth(SizeChange::AdjustProportion(-10.)),
            "<Super>minus",
        ),
        key(
            "grow-window-height",
            SetWindowHeight(SizeChange::AdjustProportion(10.)),
            "<Super><Shift>equal",
        ),
        key(
            "shrink-window-height",
            SetWindowHeight(SizeChange::AdjustProportion(-10.)),
            "<Super><Shift>minus",
        ),
        key("maximize-column", MaximizeColumn, "<Super>f"),
        key(
            "expand-column-to-available-width",
            ExpandColumnToAvailableWidth,
            "<Super><Control>f",
        ),
        key("center-column", CenterColumn, "<Super>c"),
        key(
            "center-visible-columns",
            CenterVisibleColumns,
            "<Super><Control>c",
        ),
        key(
            "toggle-column-tabbed-display",
            ToggleColumnTabbedDisplay,
            "<Super>w",
        ),
        key("toggle-window-floating", ToggleWindowFloating, "<Super>g"),
        key(
            "switch-focus-between-floating-and-tiling",
            SwitchFocusBetweenFloatingAndTiling,
            "<Super><Shift>g",
        ),
        key(
            "show-hotkey-overlay",
            ShowHotkeyOverlay,
            "<Super><Shift>slash",
        ),
        key("power-off-monitors", PowerOffMonitors, "<Super><Shift>p"),
        key("quit", Quit(false), "<Super><Shift>e"),
    ];

    // The scroll bindings. Named for the trigger rather than the action because
    // several of them bind an action that already has a key of its own above, and
    // one settings key cannot appear twice.
    //
    // mutter's accelerators are keys, so the trigger names are our extension —
    // spelled exactly as `Trigger::from_name` spells them, which is exactly as the
    // config file spells them.
    fn scroll(
        name: &str,
        action: Action,
        accels: &[&str],
        cooldown: Option<Duration>,
    ) -> (String, Action, Vec<String>, Option<Duration>) {
        (
            name.to_owned(),
            action,
            accels.iter().map(|a| (*a).to_owned()).collect(),
            cooldown,
        )
    }

    // A flick of a free-spinning wheel is many detents; without this it would
    // cross several workspaces before your hand stopped. Column moves are
    // deliberately uncapped — those you want to be able to run.
    let workspace_cooldown = Some(Duration::from_millis(150));

    let scrolls = vec![
        scroll(
            "scroll-focus-column-left",
            FocusColumnLeft,
            &["<Super>WheelScrollLeft", "<Super><Shift>WheelScrollUp"],
            None,
        ),
        scroll(
            "scroll-focus-column-right",
            FocusColumnRight,
            &["<Super>WheelScrollRight", "<Super><Shift>WheelScrollDown"],
            None,
        ),
        scroll(
            "scroll-move-column-left",
            MoveColumnLeft,
            &[
                "<Super><Control>WheelScrollLeft",
                "<Super><Control><Shift>WheelScrollUp",
            ],
            None,
        ),
        scroll(
            "scroll-move-column-right",
            MoveColumnRight,
            &[
                "<Super><Control>WheelScrollRight",
                "<Super><Control><Shift>WheelScrollDown",
            ],
            None,
        ),
        scroll(
            "scroll-focus-workspace-down",
            FocusWorkspaceDown,
            &["<Super>WheelScrollDown"],
            workspace_cooldown,
        ),
        scroll(
            "scroll-focus-workspace-up",
            FocusWorkspaceUp,
            &["<Super>WheelScrollUp"],
            workspace_cooldown,
        ),
        scroll(
            "scroll-move-column-to-workspace-down",
            MoveColumnToWorkspaceDown(true),
            &["<Super><Control>WheelScrollDown"],
            workspace_cooldown,
        ),
        scroll(
            "scroll-move-column-to-workspace-up",
            MoveColumnToWorkspaceUp(true),
            &["<Super><Control>WheelScrollUp"],
            workspace_cooldown,
        ),
    ];

    keys.into_iter()
        .map(|(name, action, accels)| (name, action, accels, None))
        .chain(scrolls)
        .collect()
}

/// The `org.gnome.mutter.wayland.keybindings` keys we honor — mutter's two
/// recovery bindings, both `META_KEY_BINDING_NON_MASKABLE`.
///
/// `switch-to-session-N` overlaps the hardcoded `XF86Switch_VT_N` path in
/// `find_bind` rather than replacing it, and deliberately: that path reads the
/// keysym the keymap produces on a real VT and so needs no settings at all,
/// which is what makes it a hatch you cannot lock yourself out of. This table
/// covers the other case, where `<Ctrl><Alt>Fn` arrives as a plain function key
/// because the keymap has no VT-switch mapping.
///
/// `xwayland-grab-access-rules` lives in the same schema but is not a
/// keybinding.
fn adopted_wayland_keybindings() -> Vec<(String, GnomeKeyAction, Vec<String>)> {
    let mut keys = vec![(
        "restore-shortcuts".to_owned(),
        GnomeKeyAction::RestoreShortcuts,
        vec!["<Super>Escape".to_owned()],
    )];

    for n in 1..=12u8 {
        keys.push((
            format!("switch-to-session-{n}"),
            GnomeKeyAction::SwitchToSession(n),
            vec![format!("<Primary><Alt>F{n}")],
        ));
    }

    keys
}

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
/// Whether `settings` differs from the last model recorded in `seen`, recording it
/// when it does.
///
/// The gsettings watcher re-reads the whole model on a change to any key in any
/// watched store, including the many keys we do not model, so most reads reproduce
/// what the consumer already has. `None` (before the initial read has been recorded)
/// counts as new, so a genuine change during startup is never swallowed.
fn is_new_model(seen: &RefCell<Option<GnomeSettings>>, settings: &GnomeSettings) -> bool {
    if seen.borrow().as_ref() == Some(settings) {
        return false;
    }
    *seen.borrow_mut() = Some(settings.clone());
    true
}

/// Which GSettings store [`Stores::open`] opens on.
///
/// The only reason this exists is so a test can drive the **real**
/// [`load_and_watch_gsettings_with`] / [`Stores::read`] / [`GnomeSettingsWriter`] instead
/// of a hand-assembled stand-in: a test that builds its own `Stores` literal is testing a
/// reimplementation of the wiring, not the wiring. The backend is created on the watcher
/// thread (glib objects are not `Send`), so this is a *kind*, not a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsStore {
    /// The process default — dconf, i.e. the user's real settings.
    System,
    /// A private in-memory store, discarded with the process. Tests only: it makes the
    /// real code path safe to run without touching the user's dconf.
    Memory,
}

impl SettingsStore {
    /// The backend to open every store on, or `None` for the process default.
    fn backend(self) -> Option<gio::SettingsBackend> {
        match self {
            SettingsStore::System => None,
            SettingsStore::Memory => Some(gio::memory_settings_backend_new()),
        }
    }
}

struct Stores {
    /// The backend every store here was opened on — `None` for the process default.
    /// Held so the *relocatable* per-folder stores, which are opened on demand rather
    /// than in [`Stores::open`], land on the same store as everything else.
    backend: Option<gio::SettingsBackend>,
    mutter: Option<gio::Settings>,
    mutter_keybindings: Option<gio::Settings>,
    /// `org.gnome.mutter.wayland.keybindings` — the two recovery bindings.
    wayland_keybindings: Option<gio::Settings>,
    /// `org.synoik.keybindings` — our own, for the scrolling-window-manager
    /// actions GNOME has no key for. `None` until the schema is installed, which
    /// leaves the compiled-in defaults in charge.
    synoik_keybindings: Option<gio::Settings>,
    shell_keybindings: Option<gio::Settings>,
    wm_keybindings: Option<gio::Settings>,
    wm_preferences: Option<gio::Settings>,
    shell: Option<gio::Settings>,
    lockdown: Option<gio::Settings>,
    screensaver: Option<gio::Settings>,
    login_screen: Option<gio::Settings>,
    background: Option<gio::Settings>,
    interface: Option<gio::Settings>,
    calendar: Option<gio::Settings>,
    notifications: Option<gio::Settings>,
    color: Option<gio::Settings>,
    input_sources: Option<gio::Settings>,
    /// `org.gnome.shell.app-switcher` — one key, `current-workspace-only`.
    app_switcher: Option<gio::Settings>,
    /// `org.gnome.shell.window-switcher` — `current-workspace-only` and `app-icon-mode`.
    window_switcher: Option<gio::Settings>,
    world_clocks: Option<gio::Settings>,
    app_folders: Option<gio::Settings>,
    /// `org.gnome.desktop.a11y` — only `always-show-universal-access-status`, the
    /// indicator's pin (`accessibility.js:10-11`).
    a11y: Option<gio::Settings>,
    a11y_interface: Option<gio::Settings>,
    a11y_applications: Option<gio::Settings>,
    a11y_keyboard: Option<gio::Settings>,
    /// `org.gnome.desktop.peripherals.*` — one store per device class, all from
    /// gsettings-desktop-schemas. See [`Peripherals`].
    touchpad: Option<gio::Settings>,
    mouse: Option<gio::Settings>,
    keyboard: Option<gio::Settings>,
    trackball: Option<gio::Settings>,
    pointingstick: Option<gio::Settings>,
    /// The relocatable `org.gnome.desktop.app-folders.folder` instances, one per
    /// `folder-children` id, opened lazily and then kept alive so the `changed`
    /// subscription installed on first sight stays live (a folder's *contents* live
    /// in its own store, so without this only `folder-children` itself would be
    /// watched).
    folder_stores: RefCell<HashMap<String, gio::Settings>>,
    /// What to call when a folder store changes — the same closure `subscribe`
    /// installed on the fixed stores, stashed so a folder store opened later can
    /// join in.
    folder_on_change: RefCell<Option<SettingsCallback>>,
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
    /// Open every schema we honor on `store`; each is `None` where not installed (e.g.
    /// running outside a GNOME environment).
    fn open(store: SettingsStore) -> Self {
        let backend = store.backend();
        let b = backend.as_ref();
        Self {
            // A refcount bump; `b` keeps borrowing it through the rest of the literal.
            backend: backend.clone(),
            mutter: gsettings("org.gnome.mutter", b),
            mutter_keybindings: gsettings("org.gnome.mutter.keybindings", b),
            wayland_keybindings: gsettings("org.gnome.mutter.wayland.keybindings", b),
            synoik_keybindings: gsettings("org.synoik.keybindings", b),
            shell_keybindings: gsettings("org.gnome.shell.keybindings", b),
            wm_keybindings: gsettings("org.gnome.desktop.wm.keybindings", b),
            wm_preferences: gsettings("org.gnome.desktop.wm.preferences", b),
            shell: gsettings("org.gnome.shell", b),
            lockdown: gsettings("org.gnome.desktop.lockdown", b),
            screensaver: gsettings("org.gnome.desktop.screensaver", b),
            login_screen: gsettings("org.gnome.login-screen", b),
            background: gsettings("org.gnome.desktop.background", b),
            interface: gsettings("org.gnome.desktop.interface", b),
            calendar: gsettings("org.gnome.desktop.calendar", b),
            notifications: gsettings("org.gnome.desktop.notifications", b),
            color: gsettings("org.gnome.settings-daemon.plugins.color", b),
            input_sources: gsettings("org.gnome.desktop.input-sources", b),
            app_switcher: gsettings("org.gnome.shell.app-switcher", b),
            window_switcher: gsettings("org.gnome.shell.window-switcher", b),
            world_clocks: gsettings("org.gnome.shell.world-clocks", b),
            app_folders: gsettings("org.gnome.desktop.app-folders", b),
            a11y: gsettings("org.gnome.desktop.a11y", b),
            a11y_interface: gsettings("org.gnome.desktop.a11y.interface", b),
            a11y_applications: gsettings("org.gnome.desktop.a11y.applications", b),
            a11y_keyboard: gsettings("org.gnome.desktop.a11y.keyboard", b),
            touchpad: gsettings("org.gnome.desktop.peripherals.touchpad", b),
            mouse: gsettings("org.gnome.desktop.peripherals.mouse", b),
            keyboard: gsettings("org.gnome.desktop.peripherals.keyboard", b),
            trackball: gsettings("org.gnome.desktop.peripherals.trackball", b),
            pointingstick: gsettings("org.gnome.desktop.peripherals.pointingstick", b),
            folder_stores: RefCell::new(HashMap::new()),
            folder_on_change: RefCell::new(None),
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
            "wm-preferences" => self.wm_preferences.as_ref(),
            "a11y-interface" => self.a11y_interface.as_ref(),
            "a11y-applications" => self.a11y_applications.as_ref(),
            "a11y-keyboard" => self.a11y_keyboard.as_ref(),
            _ => None,
        }
    }

    fn all(&self) -> impl Iterator<Item = &gio::Settings> {
        [
            &self.mutter,
            &self.mutter_keybindings,
            &self.wayland_keybindings,
            &self.synoik_keybindings,
            &self.shell_keybindings,
            &self.wm_keybindings,
            &self.wm_preferences,
            &self.shell,
            &self.lockdown,
            &self.screensaver,
            &self.login_screen,
            &self.background,
            &self.interface,
            &self.calendar,
            &self.notifications,
            &self.color,
            &self.input_sources,
            &self.app_switcher,
            &self.window_switcher,
            &self.world_clocks,
            &self.app_folders,
            &self.a11y,
            &self.a11y_interface,
            &self.a11y_applications,
            &self.a11y_keyboard,
            &self.touchpad,
            &self.mouse,
            &self.keyboard,
            &self.trackball,
            &self.pointingstick,
        ]
        .into_iter()
        .flatten()
    }

    /// GNOME's defaults overlaid with the live values of every open store.
    fn read(self: &Rc<Self>) -> GnomeSettings {
        let mut settings = GnomeSettings::default();
        if let Some(mutter) = &self.mutter {
            settings.load_mutter(mutter);
        }
        settings.peripherals = Peripherals::load(
            self.touchpad.as_ref(),
            self.mouse.as_ref(),
            self.keyboard.as_ref(),
            self.trackball.as_ref(),
            self.pointingstick.as_ref(),
        );
        settings.load_keybindings(
            self.wm_keybindings.as_ref(),
            self.mutter_keybindings.as_ref(),
            self.shell_keybindings.as_ref(),
            self.wayland_keybindings.as_ref(),
            self.synoik_keybindings.as_ref(),
        );
        if let Some(wm) = &self.wm_preferences {
            settings.load_wm_preferences(wm);
        }
        if let Some(shell) = &self.shell {
            settings.load_shell(shell);
        }
        settings.load_switchers(self.app_switcher.as_ref(), self.window_switcher.as_ref());
        if let Some(lockdown) = &self.lockdown {
            settings.load_lockdown(lockdown);
        }
        if let Some(screensaver) = &self.screensaver {
            settings.load_screensaver(screensaver);
        }
        if let Some(login_screen) = &self.login_screen {
            settings.load_login_screen(login_screen);
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
        settings.load_a11y(self);
        settings.app_folders = self.read_app_folders();
        settings
    }

    /// The `folder-children` folders, each read from its own relocatable store
    /// (`_redisplay`, `appDisplay.js:1510-1533`).
    fn read_app_folders(self: &Rc<Self>) -> Vec<AppFolder> {
        let Some(folders) = &self.app_folders else {
            return Vec::new();
        };
        if !settings_has_key(folders, "folder-children") {
            return Vec::new();
        }
        folders
            .strv("folder-children")
            .iter()
            .filter_map(|id| {
                let store = self.folder_store(id.as_str())?;
                let name = store.string("name").to_string();
                let name = if store.boolean("translate") {
                    translated_folder_name(&name).unwrap_or(name)
                } else {
                    name
                };
                Some(AppFolder {
                    id: id.to_string(),
                    name,
                    categories: strv(&store, "categories"),
                    apps: strv(&store, "apps"),
                    excluded_apps: strv(&store, "excluded-apps"),
                })
            })
            .collect()
    }

    /// The relocatable store for one folder id, opened on first sight and cached.
    ///
    /// It is cached because it is also *subscribed*: a folder's keys live here, not
    /// in `org.gnome.desktop.app-folders`, so dropping the handle would leave every
    /// change to a folder's contents unwatched.
    fn folder_store(self: &Rc<Self>, id: &str) -> Option<gio::Settings> {
        if let Some(store) = self.folder_stores.borrow().get(id) {
            return Some(store.clone());
        }
        // On the backend the rest of the stores were opened on — a relocatable store
        // opened on the *default* backend would read (and a write would clobber) the
        // user's real dconf even when everything else is on a private store.
        let store = folder_settings(id, self.backend.as_ref())?;
        if let Some(on_change) = self.folder_on_change.borrow().clone() {
            let stores = self.clone();
            store.connect_changed(None, move |_, _key| {
                on_change(stores.read());
            });
        }
        self.folder_stores
            .borrow_mut()
            .insert(id.to_owned(), store.clone());
        Some(store)
    }

    /// Invoke `on_change` with a freshly-read model whenever any key in any
    /// store changes. The subscriptions live as long as the stores do.
    fn subscribe(self: &Rc<Self>, on_change: impl Fn(GnomeSettings) + 'static) {
        let on_change: SettingsCallback = Rc::new(on_change);
        // Stash it before the first read: `read` opens the per-folder stores lazily,
        // and each wants this same subscription (see `folder_store`).
        *self.folder_on_change.borrow_mut() = Some(on_change.clone());
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

/// What a store's `changed` subscription runs with the freshly-read model.
type SettingsCallback = Rc<dyn Fn(GnomeSettings)>;

/// One entry of GNOME's `DEFAULT_FOLDERS` table (`appDisplay.js:60-77`), as
/// [`GnomeSettingsWriter::ensure_default_folders`] writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultFolder {
    /// The `folder-children` id.
    pub id: &'static str,
    /// The `.directory` file the folder's `name` points at. Seeded folders always get
    /// `translate` set, so this is never shown raw — see [`translated_folder_name`].
    pub directory: &'static str,
    pub categories: &'static [&'static str],
    /// The default members, already filtered to installed apps by the caller.
    pub apps: Vec<String>,
}

/// GNOME's `DEFAULT_FOLDERS` (`appDisplay.js:60-77`) with the app lists 50.1 generates
/// from `data/default-apps/{system,utilities}-folder.txt` at build time, filtered by
/// `is_installed` the way `_ensureDefaultFolders` filters with `lookup_app`.
///
/// `System` and `Utilities` are explicit app lists; `YaST` and `Pardus` are
/// distro category folders that resolve to nothing on most machines — GNOME writes
/// all four regardless and lets the empty ones go undisplayed.
pub fn default_folders(is_installed: impl Fn(&str) -> bool) -> Vec<DefaultFolder> {
    // `data/default-apps/system-folder.txt` and `utilities-folder.txt`, in file order.
    const SYSTEM_APPS: &[&str] = &[
        "nm-connection-editor.desktop",
        "org.gnome.DejaDup.desktop",
        "org.gnome.baobab.desktop",
        "org.gnome.DiskUtility.desktop",
        "org.gnome.Logs.desktop",
        "org.freedesktop.MalcontentControl.desktop",
        "org.freedesktop.GnomeAbrt.desktop",
        "org.gnome.Sysprof.desktop",
        "org.gnome.SystemMonitor.desktop",
        "org.gnome.tweaks.desktop",
    ];
    const UTILITIES_APPS: &[&str] = &[
        "org.gnome.Decibels.desktop",
        "org.gnome.Connections.desktop",
        "org.gnome.Papers.desktop",
        "org.gnome.FileRoller.desktop",
        "org.gnome.font-viewer.desktop",
        "org.gnome.Loupe.desktop",
        // Both seahorse ids ship in the list: the old desktop name and the new one.
        "org.gnome.seahorse.Application.desktop",
        "org.gnome.Seahorse.desktop",
        "org.gnome.Showtime.desktop",
    ];
    let filter = |apps: &[&str]| -> Vec<String> {
        apps.iter()
            .filter(|id| is_installed(id))
            .map(|id| (*id).to_owned())
            .collect()
    };
    vec![
        DefaultFolder {
            id: "System",
            directory: "X-GNOME-Shell-System.directory",
            categories: &[],
            apps: filter(SYSTEM_APPS),
        },
        DefaultFolder {
            id: "Utilities",
            directory: "X-GNOME-Shell-Utilities.directory",
            categories: &[],
            apps: filter(UTILITIES_APPS),
        },
        DefaultFolder {
            id: "YaST",
            directory: "suse-yast.directory",
            categories: &["X-SuSE-YaST"],
            apps: Vec::new(),
        },
        DefaultFolder {
            id: "Pardus",
            directory: "X-Pardus-Apps.directory",
            categories: &["X-Pardus-Apps"],
            apps: Vec::new(),
        },
    ]
}

/// Whether `folder-children` has never been seeded — `_ensureDefaultFolders`'s guard
/// (`appDisplay.js:1407-1409`), as a function of the two things it reads so it can be
/// tested without a store.
fn app_folders_need_seeding(has_user_value: bool, children: &[String]) -> bool {
    !has_user_value && children.is_empty()
}

/// The store half of [`GnomeSettingsWriter::ensure_default_folders`].
///
/// `backend` is the one the parent store was opened on — `None` for the real dconf.
/// The four per-folder stores **must** be opened on that same backend, or a test
/// handing this a memory-backed parent would still write the children into the running
/// user's real dconf.
fn ensure_default_folders(
    app_folders: &gio::Settings,
    folders: &[DefaultFolder],
    backend: Option<&gio::SettingsBackend>,
) {
    if !settings_has_key(app_folders, "folder-children") {
        return;
    }
    let children = strv(app_folders, "folder-children");
    if !app_folders_need_seeding(
        app_folders.user_value("folder-children").is_some(),
        &children,
    ) {
        return;
    }
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return;
    };
    let Some(schema) = source.lookup(APP_FOLDER_SCHEMA, true) else {
        return;
    };
    let ids: Vec<&str> = folders.iter().map(|f| f.id).collect();
    if let Err(err) = app_folders.set_strv("folder-children", ids) {
        warn!("error seeding org.gnome.desktop.app-folders folder-children: {err}");
        return;
    }
    for folder in folders {
        let path = format!("/org/gnome/desktop/app-folders/folders/{}/", folder.id);
        let store = gio::Settings::new_full(&schema, backend, Some(&path));
        let _ = store.set_string("name", folder.directory);
        let _ = store.set_boolean("translate", true);
        if !folder.categories.is_empty() {
            let _ = store.set_strv("categories", folder.categories);
        }
        if !folder.apps.is_empty() {
            let apps: Vec<&str> = folder.apps.iter().map(String::as_str).collect();
            let _ = store.set_strv("apps", apps);
        }
    }
}

/// Make a folder: append `id` to `folder-children` and write its name and apps into the
/// relocatable store at its own path (`createFolder`, `appDisplay.js:1699-1742`).
///
/// `translate` stays **false** — the name is either a category's already-translated
/// `.directory` title or the literal "Unnamed Folder", and in both cases it is the string
/// to show, not a `.directory` basename to look up (which is what `translate` means, and
/// what the *seeded* default folders set).
fn create_app_folder(
    app_folders: &gio::Settings,
    id: &str,
    name: &str,
    apps: &[String],
    backend: Option<&gio::SettingsBackend>,
) -> bool {
    if !settings_has_key(app_folders, "folder-children") {
        return false;
    }
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return false;
    };
    let Some(schema) = source.lookup(APP_FOLDER_SCHEMA, true) else {
        return false;
    };
    let path = format!("/org/gnome/desktop/app-folders/folders/{id}/");
    let store = gio::Settings::new_full(&schema, backend, Some(&path));
    let apps: Vec<&str> = apps.iter().map(String::as_str).collect();
    if let Err(err) = store.set_string("name", name) {
        warn!("error naming the new app folder {id}: {err}");
        return false;
    }
    let _ = store.set_boolean("translate", false);
    if let Err(err) = store.set_strv("apps", apps) {
        warn!("error filling the new app folder {id}: {err}");
        return false;
    }
    // Last: the folder only exists once it is a child, so a half-written store is never
    // visible as an empty folder.
    let mut children = strv(app_folders, "folder-children");
    children.push(id.to_owned());
    let children: Vec<&str> = children.iter().map(String::as_str).collect();
    if let Err(err) = app_folders.set_strv("folder-children", children) {
        warn!("error adding {id} to folder-children: {err}");
        return false;
    }
    true
}

/// Add `app` to the folder `id`'s `apps`, and take it off its `excluded-apps` if it was
/// listed there — which only a categories-based folder ever has (`FolderView.addApp`,
/// `appDisplay.js:2223-2236`).
fn add_to_app_folder(id: &str, app: &str, backend: Option<&gio::SettingsBackend>) -> bool {
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return false;
    };
    let Some(schema) = source.lookup(APP_FOLDER_SCHEMA, true) else {
        return false;
    };
    let path = format!("/org/gnome/desktop/app-folders/folders/{id}/");
    let store = gio::Settings::new_full(&schema, backend, Some(&path));

    let mut apps = strv(&store, "apps");
    if apps.iter().any(|a| a == app) {
        return false;
    }
    apps.push(app.to_owned());
    let refs: Vec<&str> = apps.iter().map(String::as_str).collect();
    if let Err(err) = store.set_strv("apps", refs) {
        warn!("error adding {app} to the app folder {id}: {err}");
        return false;
    }

    let excluded = strv(&store, "excluded-apps");
    if excluded.iter().any(|a| a == app) {
        let kept: Vec<&str> = excluded
            .iter()
            .map(String::as_str)
            .filter(|a| *a != app)
            .collect();
        let _ = store.set_strv("excluded-apps", kept);
    }
    true
}

/// Take `app` off the folder `id`'s `apps`, and — for a categories-based folder — push it
/// onto `excluded-apps`, which is the only thing that keeps the sweep from bringing it
/// straight back (`FolderView.removeApp`, `appDisplay.js:2263-2271`).
fn remove_from_app_folder(id: &str, app: &str, backend: Option<&gio::SettingsBackend>) -> bool {
    let Some(store) = folder_settings(id, backend) else {
        return false;
    };
    let apps = strv(&store, "apps");
    let kept: Vec<&str> = apps
        .iter()
        .map(String::as_str)
        .filter(|a| *a != app)
        .collect();
    if kept.len() != apps.len() {
        if let Err(err) = store.set_strv("apps", kept) {
            warn!("error removing {app} from the app folder {id}: {err}");
            return false;
        }
    }
    if !strv(&store, "categories").is_empty() {
        let mut excluded = strv(&store, "excluded-apps");
        if !excluded.iter().any(|a| a == app) {
            excluded.push(app.to_owned());
            let refs: Vec<&str> = excluded.iter().map(String::as_str).collect();
            let _ = store.set_strv("excluded-apps", refs);
        }
    }
    true
}

/// Delete the folder `id`: reset every key of its relocatable store — which is what makes
/// the store itself go away — and drop the id from `folder-children`
/// (`appDisplay.js:2245-2262`).
fn delete_app_folder(
    app_folders: &gio::Settings,
    id: &str,
    backend: Option<&gio::SettingsBackend>,
) -> bool {
    if !settings_has_key(app_folders, "folder-children") {
        return false;
    }
    if let Some(store) = folder_settings(id, backend) {
        for key in store
            .settings_schema()
            .map(|s| s.list_keys())
            .unwrap_or_default()
        {
            store.reset(&key);
        }
    }
    let children = strv(app_folders, "folder-children");
    let kept: Vec<&str> = children
        .iter()
        .map(String::as_str)
        .filter(|c| *c != id)
        .collect();
    if kept.len() == children.len() {
        return false;
    }
    if let Err(err) = app_folders.set_strv("folder-children", kept) {
        warn!("error removing the app folder {id} from folder-children: {err}");
        return false;
    }
    true
}

/// The relocatable store for one folder id, or `None` if the schema is not installed.
fn folder_settings(id: &str, backend: Option<&gio::SettingsBackend>) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(APP_FOLDER_SCHEMA, true)?;
    let path = format!("/org/gnome/desktop/app-folders/folders/{id}/");
    Some(gio::Settings::new_full(&schema, backend, Some(&path)))
}

/// Adopt the user's locale for **collation only**, so GLib sorts names the way the
/// session does (`g_utf8_collate_key` reads `LC_COLLATE`, and a process that never calls
/// `setlocale` is in the C locale, where sorting is codepoint order and "Écran" lands
/// after "Zip"). gnome-shell gets the equivalent from ICU via `localeCompare`.
///
/// `LC_COLLATE` and not `LC_ALL`: `LC_NUMERIC` in particular changes the decimal
/// separator that C libraries in this process parse and print with — a well-known way to
/// break shader and config parsing in the graphics stack for a sorting fix.
pub fn init_collation() {
    // SAFETY: called once, before any thread is spawned.
    unsafe {
        libc::setlocale(libc::LC_COLLATE, c"".as_ptr());
    }
}

/// The relocatable per-folder schema, one instance per `folder-children` id
/// (`appDisplay.js:2295-2299`).
const APP_FOLDER_SCHEMA: &str = "org.gnome.desktop.app-folders.folder";

/// An `as` key as owned `String`s.
fn strv(settings: &gio::Settings, key: &str) -> Vec<String> {
    settings.strv(key).iter().map(|s| s.to_string()).collect()
}

/// Open a [`gio::Settings`] for `schema_id` on `backend`, or `None` if the schema isn't
/// installed (e.g. running outside a GNOME environment). Guarding with the schema
/// source avoids `gio::Settings::new`'s abort-on-missing-schema behavior.
///
/// `backend` is `None` for the process default (dconf — the user's real store); tests
/// pass a memory backend so they exercise this same code against a private store. See
/// [`SettingsStore`].
fn gsettings(schema_id: &str, backend: Option<&gio::SettingsBackend>) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(schema_id, true)?;
    Some(gio::Settings::new_full(&schema, backend, None))
}

/// Whether an application `${id}.desktop` is installed, gnome-shell's
/// `Shell.AppSystem.lookup_app` as used by `WorldClocksSection._sync`. Scans the
/// XDG `applications` dirs for a flat `<id>` file (nested / vendor-prefixed
/// desktop-id resolution is a recorded divergence — fine for `org.gnome.clocks`).
fn desktop_app_installed(desktop_id: &str) -> bool {
    xdg_data_dirs()
        .iter()
        .any(|dir| dir.join("applications").join(desktop_id).exists())
}

/// The XDG data search path, **user directory first** — the order every
/// first-wins lookup below walks (`shell-app-cache.c:112-140`).
fn xdg_data_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(PathBuf::from(data_home));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share"));
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .unwrap_or_else(|| std::ffi::OsString::from("/usr/local/share:/usr/share"));
    dirs.extend(std::env::split_paths(&data_dirs));
    dirs
}

/// A fresh folder id — a random uuid, as in `createFolder` (`appDisplay.js:1700`).
pub fn new_folder_id() -> String {
    glib::uuid_string_random().to_string()
}

/// The name for a folder made out of `apps` — the first category common to **every**
/// one of them whose `<category>.directory` has a translated title, else `None` for the
/// caller's "Unnamed Folder" (`_findBestFolderName`, `appDisplay.js:114-144`).
///
/// "First" is in the order the *first* app lists them, which is what GNOME's reduce
/// produces: a category is pushed the moment its counter reaches the app count, so the
/// order is that of the last app to complete each one — for two apps, the second app's
/// order. Ours walks the first app's list, which agrees whenever the categories are
/// listed in the same order (the usual case) and is otherwise an arbitrary tie-break
/// between equally-common categories.
pub fn best_folder_name(apps: &[Vec<String>]) -> Option<String> {
    let first = apps.first()?;
    first.iter().find_map(|category| {
        if category.is_empty() || !apps.iter().all(|a| a.contains(category)) {
            return None;
        }
        translated_folder_name(&format!("{category}.directory"))
    })
}

/// The translated display name of an app folder whose `translate` key is set —
/// `shell_util_get_translated_folder_name` (`src/shell-app-cache.c:95-147`).
///
/// `name` is a `.directory` file name (e.g. `"X-GNOME-Utilities.directory"`), looked
/// up under `desktop-directories/` in each XDG data dir, user dir first; the answer
/// is its `[Desktop Entry] Name` as a **locale** string. First file found wins, even
/// if it has no `Name` — GNOME caches the first entry it managed to add.
fn translated_folder_name(name: &str) -> Option<String> {
    translated_folder_name_in(&xdg_data_dirs(), name)
}

/// [`translated_folder_name`] against an explicit search path — the lookup, with no
/// environment read, so a test can point it at a fixture directory.
fn translated_folder_name_in(dirs: &[PathBuf], name: &str) -> Option<String> {
    for dir in dirs {
        let path = dir.join("desktop-directories").join(name);
        let keyfile = glib::KeyFile::new();
        if keyfile
            .load_from_file(&path, glib::KeyFileFlags::NONE)
            .is_err()
        {
            continue;
        }
        return keyfile
            .locale_string("Desktop Entry", "Name", None)
            .ok()
            .map(|s| s.to_string());
    }
    None
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
    /// A non-keyboard trigger — a mouse button, a scroll direction, a tablet
    /// stylus button — spelled by name, e.g. `<Super>WheelScrollDown`.
    ///
    /// **Ours, not GNOME's.** mutter's accelerators are keys only; this exists
    /// because a scrolling window manager binds the scroll wheel, and it would
    /// otherwise be the one part of the model with nowhere to live. Never holds
    /// [`Trigger::Keysym`] — [`Trigger::from_name`] does not produce it.
    Device(synoik_config::Trigger),
}

/// One parsed keyboard accelerator — a single entry of a keybinding's
/// GSettings array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accel {
    pub trigger: AccelTrigger,
    pub mods: AccelMods,
}

/// `Above_Tab` resolves to the physical key above Tab: evdev `KEY_GRAVE`
/// (0x29) plus the xkb keycode offset.
///
/// **A keycode, not a keysym, and not layout-dependent** — mutter special-cases its fake
/// `META_KEY_ABOVE_TAB` to exactly `KEY_GRAVE + 8` and returns before consulting any layout
/// (`add_keycodes_for_keysym`, `src/core/keybindings.c:385-392`). Matching by *position* is what
/// makes it layout-independent: on AZERTY that key types `²`, and the binding still works because
/// nothing ever looks at what it types.
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

    let trigger = if let Some(trigger) = synoik_config::Trigger::from_name(rest) {
        // Checked before the keysym lookup, which is safe because none of these
        // names is a keysym (nor an XF86 one, which is what the retry below would
        // otherwise turn them into).
        AccelTrigger::Device(trigger)
    } else if let Some(keycode) = parse_accel_keycode(rest) {
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

/// The inverse of [`read_app_picker_layout`]: pages of app ids → the `aa{sv}` the key
/// holds. `ToVariant for Variant` boxes, which is exactly what the `v` needs — at both
/// levels, since the per-app value is itself a boxed `a{sv}`.
fn build_app_picker_layout(pages: &[Vec<String>]) -> glib::Variant {
    use glib::prelude::ToVariant;
    let pages: Vec<HashMap<String, glib::Variant>> = pages
        .iter()
        .map(|page| {
            page.iter()
                .enumerate()
                .map(|(position, id)| {
                    let props: HashMap<String, glib::Variant> =
                        [("position".to_owned(), (position as i32).to_variant())].into();
                    (id.clone(), props.to_variant())
                })
                .collect()
        })
        .collect();
    pages.to_variant()
}

/// Unpack `app-picker-layout` (`aa{sv}`: one dict per page, desktop id → a boxed
/// `{'position': <int32>}`) into a flat id → `(page, position)` map — the shape
/// `PageManager.getAppPosition` answers in (`appDisplay.js:1276-1291`).
///
/// An entry with no readable `position` is skipped rather than defaulted: a zero would
/// silently jump it to the front of its page.
fn read_app_picker_layout(value: &glib::Variant) -> HashMap<String, (usize, i32)> {
    let mut out = HashMap::new();
    for (page, page_value) in value.iter().enumerate() {
        for (id, props) in page_value.iter().filter_map(|e| {
            let id = e.child_value(0).str()?.to_owned();
            // Both `v`s have to be opened: the per-app value boxes the property dict,
            // and the property value boxes the int. Iterating the box instead of the
            // dict yields the dict itself as a single "entry" whose key is not a
            // string, so every lookup silently misses and the whole key reads empty.
            Some((id, e.child_value(1).as_variant()?))
        }) {
            let Some(position) = props
                .iter()
                .find(|kv| kv.child_value(0).str() == Some("position"))
                .and_then(|kv| kv.child_value(1).as_variant())
                .and_then(|v| v.get::<i32>())
            else {
                continue;
            };
            out.insert(id, (page, position));
        }
    }
    out
}

/// The point size out of a Pango font description like `"Cantarell 12"` or
/// `"Cantarell Bold Italic 11.5"` — the trailing number, which is all
/// `pango_font_description_from_string` takes as the size (`st-theme-context.c:243`).
/// `None` when there is no trailing size, in which case the caller keeps the default
/// rather than guessing.
fn parse_font_size_pt(desc: &str) -> Option<f64> {
    let last = desc.rsplit(' ').next()?;
    let pt = last.parse::<f64>().ok()?;
    (pt > 0.).then_some(pt)
}

/// The family out of a Pango font description like `"Adwaita Sans 11"` or
/// `"Source Sans Pro Semibold 10.5"`: everything left after dropping the trailing size and any
/// trailing style words, which is how `pango_font_description_from_string` splits it.
///
/// Ambiguity is Pango's, not ours: a description's style words are just trailing words off a
/// known list, so a family whose own last word is on that list (`"Roboto Condensed"`) parses as
/// `Roboto` + condensed. Pango resolves it the same way, so matching it is the point.
///
/// `None` when nothing is left (a description that is only a size, or empty), in which case the
/// caller keeps the default rather than asking for a nameless family.
fn parse_font_family(desc: &str) -> Option<String> {
    // Pango matches style words case-insensitively and ignores the dashes, so `Semi-Bold`,
    // `semibold` and `Semi Bold`'s halves all land here.
    const STYLE_WORDS: &[&str] = &[
        // styles and variants
        "normal",
        "roman",
        "oblique",
        "italic",
        "smallcaps",
        "allsmallcaps",
        "unicase",
        "titlecaps",
        "petitecaps",
        "allpetitecaps",
        // weights
        "thin",
        "ultralight",
        "extralight",
        "light",
        "semilight",
        "demilight",
        "book",
        "regular",
        "medium",
        "semibold",
        "demibold",
        "bold",
        "ultrabold",
        "extrabold",
        "heavy",
        "black",
        "ultraheavy",
        "extrablack",
        "ultrablack",
        // stretch
        "ultracondensed",
        "extracondensed",
        "condensed",
        "semicondensed",
        "semiexpanded",
        "expanded",
        "extraexpanded",
        "ultraexpanded",
    ];

    let mut words: Vec<&str> = desc.split_whitespace().collect();
    if words.last().is_some_and(|w| w.parse::<f64>().is_ok()) {
        words.pop();
    }
    while words.last().is_some_and(|w| {
        let normalized = w.replace('-', "").to_ascii_lowercase();
        STYLE_WORDS.contains(&normalized.as_str())
    }) {
        words.pop();
    }
    (!words.is_empty()).then(|| words.join(" "))
}

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
    /// No accelerator in our own schema may take a chord we adopt from GNOME.
    ///
    /// This is the fork tenet made mechanical: where the two models want the same
    /// key, GNOME's wins, so ours must not ask for it in the first place. Matching
    /// order already favours GNOME, but a collision would mean a key in our schema
    /// that silently does nothing — a setting you can change with no effect, which
    /// is worse than one that isn't there.
    #[test]
    fn synoik_accels_do_not_collide_with_gnome() {
        let mut gnome: Vec<(Accel, String)> = Vec::new();
        for (key, _, defaults) in adopted_wm_keybindings()
            .into_iter()
            .chain(adopted_mutter_keybindings())
            .chain(adopted_shell_keybindings())
            .chain(adopted_wayland_keybindings())
        {
            for accel in parse_accels(&key, defaults) {
                gnome.push((accel, key.clone()));
            }
        }

        let mut clashes = Vec::new();
        for (key, _, defaults, _) in adopted_synoik_keybindings() {
            for accel in parse_accels(&key, defaults) {
                if let Some((_, theirs)) = gnome.iter().find(|(a, _)| *a == accel) {
                    clashes.push(format!("{key} wants {accel:?}, which is GNOME's {theirs}"));
                }
            }
        }
        assert!(clashes.is_empty(), "{clashes:#?}");
    }

    /// Pull `(key name, accelerator)` pairs out of a `.gschema.xml`'s array defaults.
    ///
    /// Deliberately dumb, but not naive about the three ways these files vary: attribute
    /// order (`<key name= type=>` vs `<key type= name=>`), quote style (mutter uses
    /// `'…'`, gnome-shell uses `"…"`), and CDATA — `org.gnome.desktop.wm.keybindings`
    /// wraps every default in `<![CDATA[…]]>`. Missing any of them silently shrinks the
    /// set a collision is checked against, which is how a guard passes while being blind;
    /// the caller asserts on the count for exactly that reason.
    fn schema_default_accels(xml: &str) -> Vec<(String, String)> {
        let mut rv = Vec::new();
        for chunk in xml.split("<key ").skip(1) {
            let Some(elem) = chunk.split("</key>").next() else {
                continue;
            };
            let Some(name) = elem
                .split("name=\"")
                .nth(1)
                .and_then(|r| r.split('"').next())
            else {
                continue;
            };
            let Some(default) = elem
                .split("<default>")
                .nth(1)
                .and_then(|r| r.split("</default>").next())
            else {
                continue;
            };
            let default = default
                .replace("<![CDATA[", "")
                .replace("]]>", "")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&quot;", "\"");
            let default = default.trim();
            // Only array-typed keys are accelerator lists.
            if !default.starts_with('[') {
                continue;
            }
            let mut rest = default;
            while let Some(open) = rest.find(['\'', '"']) {
                let quote = rest.as_bytes()[open] as char;
                let after = &rest[open + 1..];
                let Some(close) = after.find(quote) else {
                    break;
                };
                let value = &after[..close];
                if !value.is_empty() {
                    rv.push((name.to_owned(), value.to_owned()));
                }
                rest = &after[close + 1..];
            }
        }
        rv
    }

    /// No accelerator of ours may take a chord GNOME *ships*, adopted or not.
    ///
    /// [`synoik_accels_do_not_collide_with_gnome`] only compares against the keys we adopt,
    /// which leaves two blind spots that both turned out to be real: GNOME keys we
    /// deliberately deferred still have defaults (`minimize` is `<Super>h`), and
    /// gnome-settings-daemon's media keys are not in any table of ours at all
    /// (`screensaver` is `<Super>l` — the lock key). Ours would win both, because the
    /// settings model resolves ahead of external accelerator grabs, so taking one does not
    /// produce a dead key of ours: it silently disables GNOME's.
    ///
    /// Comparison goes through `parse_accelerator`, so modifier order and spelling
    /// (`<Primary>` vs `<Control>`, `<Alt><Super>` vs `<Super><Alt>`) cannot hide a clash.
    #[test]
    fn synoik_accels_do_not_collide_with_anything_gnome_ships() {
        let vendored = [
            (
                "org.gnome.desktop.wm.keybindings",
                include_str!("../resources/schemas/org.gnome.desktop.wm.keybindings.gschema.xml"),
            ),
            (
                "org.gnome.mutter",
                include_str!("../resources/schemas/org.gnome.mutter.gschema.xml"),
            ),
            (
                "org.gnome.mutter.wayland",
                include_str!("../resources/schemas/org.gnome.mutter.wayland.gschema.xml"),
            ),
            (
                "org.gnome.shell",
                include_str!("../resources/schemas/org.gnome.shell.gschema.xml"),
            ),
        ];

        let mut theirs: Vec<(Accel, String)> = Vec::new();
        let mut sources = 0;
        let mut add = |schema: &str, xml: &str, sources: &mut usize| {
            *sources += 1;
            for (key, accel) in schema_default_accels(xml) {
                for parsed in parse_accels(&key, vec![accel]) {
                    theirs.push((parsed, format!("{schema} {key}")));
                }
            }
        };
        for (schema, xml) in vendored {
            add(schema, xml, &mut sources);
        }
        // gnome-settings-daemon is a package we do not replace, so its schema is not
        // vendored — read the installed one where it exists.
        let gsd = "/usr/share/glib-2.0/schemas/\
                   org.gnome.settings-daemon.plugins.media-keys.gschema.xml"
            .replace(char::is_whitespace, "");
        match std::fs::read_to_string(&gsd) {
            Ok(xml) => add(
                "org.gnome.settings-daemon.plugins.media-keys",
                &xml,
                &mut sources,
            ),
            // Not optional: the media keys are a real part of the keymap this test compares, and
            // "unchecked" is indistinguishable from "checked and fine" in a green run. Install
            // gnome-settings-daemon (Fedora) / gnome-settings-daemon-common (Debian, Ubuntu).
            Err(err) => panic!(
                "gsd media-keys schema is not installed ({gsd}): {err}\n\
                 This test compares our accelerators against GNOME's, and gnome-settings-daemon \
                 owns the media-key half of them — without it the comparison silently comes up \
                 short instead of failing.",
            ),
        }

        assert!(
            theirs.len() > 100,
            "only {} accelerators parsed out of {sources} schemas — the extractor is blind, \
             not GNOME's keymap empty",
            theirs.len(),
        );

        let mut clashes = Vec::new();
        for (key, _, defaults, _) in adopted_synoik_keybindings() {
            for accel in parse_accels(&key, defaults) {
                if let Some((_, theirs)) = theirs.iter().find(|(a, _)| *a == accel) {
                    clashes.push(format!(
                        "{key} wants {accel:?}, which GNOME ships as {theirs}"
                    ));
                }
            }
        }
        assert!(clashes.is_empty(), "{clashes:#?}");
    }

    /// Every default in our schema must actually parse.
    ///
    /// `parse_accels` is deliberately forgiving — mutter's `update_binding` warns
    /// about a bad accelerator and keeps the rest of the key — so a typo in a
    /// keysym name (`bracketleft`, `slash`, `period`) does not fail anything. It
    /// just yields a binding with no accelerators, i.e. a key that quietly does
    /// nothing. Our own defaults are not user input and have no excuse.
    #[test]
    fn our_defaults_all_parse() {
        for (key, _, defaults, _) in adopted_synoik_keybindings() {
            let want = defaults.len();
            let got = parse_accels(&key, defaults).len();
            assert_eq!(got, want, "{key} has an accelerator that does not parse");
        }
    }

    /// The table and the `.gschema.xml` that ships beside it must name the same
    /// keys with the same defaults.
    ///
    /// They are two hand-written copies of one list: the table is what runs where
    /// the schema isn't installed, the XML is what `gsettings` and the Settings UI
    /// see. Drift between them is invisible until someone edits a key that turns
    /// out not to be read, so it is checked rather than trusted.
    #[test]
    fn our_schema_matches_the_table() {
        let xml = include_str!("../resources/schemas/org.synoik.keybindings.gschema.xml");

        // A deliberately dumb reader: enough of the file's shape to compare, and no
        // XML dependency for one test.
        let mut in_file = Vec::new();
        for chunk in xml.split("<key name=\"").skip(1) {
            let (name, rest) = chunk.split_once('"').expect("key name is quoted");
            let (_, rest) = rest.split_once("<default>").expect("key has a default");
            let (default, _) = rest.split_once("</default>").expect("default is closed");
            let accels: Vec<String> = default
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|s| s.trim().trim_matches('\'').to_owned())
                .filter(|s| !s.is_empty())
                .collect();
            in_file.push((name.to_owned(), accels));
        }

        let in_table: Vec<(String, Vec<String>)> = adopted_synoik_keybindings()
            .into_iter()
            .map(|(key, _, defaults, _)| (key, defaults))
            .collect();

        assert_eq!(
            in_file, in_table,
            "resources/schemas/org.synoik.keybindings.gschema.xml and \
             adopted_synoik_keybindings() have drifted apart"
        );
    }

    /// The vendored GNOME schemas must be byte-identical to the installed originals.
    ///
    /// We ship copies of the mutter and gnome-shell schemas so the session still has
    /// *editable* keybindings once those packages are replaced — without them `gsettings
    /// set` fails, dconf-editor shows nothing, and the Keyboard panel cannot enumerate a
    /// shortcut. A copy is only worth having if it is the same file, and the way it stops
    /// being the same file is a GNOME upgrade, which changes nothing here and so says
    /// nothing.
    ///
    /// Skipped where the originals are not installed: the copies exist precisely for that
    /// case, and there is nothing to compare against.
    #[test]
    fn vendored_schemas_match_the_installed_ones() {
        let vendored = [
            (
                "org.gnome.mutter.gschema.xml",
                include_str!("../resources/schemas/org.gnome.mutter.gschema.xml"),
            ),
            (
                "org.gnome.mutter.wayland.gschema.xml",
                include_str!("../resources/schemas/org.gnome.mutter.wayland.gschema.xml"),
            ),
            (
                "org.gnome.shell.gschema.xml",
                include_str!("../resources/schemas/org.gnome.shell.gschema.xml"),
            ),
            (
                "org.gnome.desktop.wm.keybindings.gschema.xml",
                include_str!("../resources/schemas/org.gnome.desktop.wm.keybindings.gschema.xml"),
            ),
        ];

        let mut compared = 0;
        for (name, ours) in vendored {
            let path = std::path::Path::new("/usr/share/glib-2.0/schemas").join(name);
            let Ok(theirs) = std::fs::read_to_string(&path) else {
                continue;
            };
            assert_eq!(
                ours,
                theirs,
                "resources/schemas/{name} has drifted from {}; re-copy it and re-check the \
                 keys we read out of it",
                path.display(),
            );
            compared += 1;
        }

        if compared == 0 {
            eprintln!("no GNOME schemas installed; nothing to compare the vendored copies to");
        }
    }

    /// Every key in the shipped `.gschema.override` must match the fallback table.
    ///
    /// The override is what a *session* gets — it tunes the vendored GNOME schemas in our
    /// own schema directory. The tables are what the compositor runs on when there are no
    /// schemas at all, which is also what the conformance corpus asserts against. A
    /// divergence written in one and not the other is a session that behaves differently
    /// from every test.
    ///
    /// Only the `org.gnome.desktop.wm.keybindings` group is checked: the `org.gnome.mutter`
    /// group carries settings we do not read as keybindings.
    #[test]
    fn override_matches_the_tables() {
        let text = include_str!("../resources/schemas/synoik.gschema.override");

        let mut group = "";
        let mut checked = 0;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                group = match name {
                    "org.gnome.desktop.wm.keybindings" => name,
                    _ => "",
                };
                continue;
            }
            if group.is_empty() {
                continue;
            }

            let (key, value) = line.split_once('=').expect("a key line is key=value");
            let want: Vec<String> = value
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|s| s.trim().trim_matches('\'').to_owned())
                .filter(|s| !s.is_empty())
                .collect();

            let got = adopted_wm_keybindings()
                .into_iter()
                .find(|(name, ..)| name == key)
                .unwrap_or_else(|| panic!("the override sets {key}, which no table names"))
                .2;

            assert_eq!(
                got, want,
                "{group} {key} differs between the override and the table"
            );
            checked += 1;
        }

        assert!(checked > 0, "the override parser matched nothing at all");
    }

    /// The brightness keys come from `org.gnome.shell.keybindings`, which we had never read
    /// before: they are the shell's own bindings, not the wm's. Their defaults are the bare
    /// `XF86MonBrightness*` keysyms, with the `-monitor` variants on Shift
    /// (`data/org.gnome.shell.gschema.xml.in:281-304`).
    #[test]
    fn brightness_keys_are_in_the_default_keybindings() {
        use crate::brightness::Step;

        let bindings = default_keybindings();
        let find = |step, current_monitor| {
            bindings
                .iter()
                .find(|kb| {
                    kb.action.gnome()
                        == Some(GnomeKeyAction::ScreenBrightness {
                            step,
                            current_monitor,
                        })
                })
                .unwrap_or_else(|| panic!("no binding for {step:?} monitor={current_monitor}"))
        };

        // One accel each, and the plain/`-monitor` pair differs only by Shift.
        for step in [Step::Up, Step::Down, Step::Cycle] {
            let plain = find(step, false);
            let monitor = find(step, true);
            assert_eq!(plain.accels.len(), 1);
            assert_eq!(monitor.accels.len(), 1);
            assert_ne!(
                plain.accels[0], monitor.accels[0],
                "the -monitor variant must not shadow the plain one"
            );
        }

        // The three steps are distinct bindings, not one key reused.
        assert_ne!(
            find(Step::Up, false).accels[0],
            find(Step::Down, false).accels[0]
        );
        assert_ne!(
            find(Step::Up, false).accels[0],
            find(Step::Cycle, false).accels[0]
        );
    }

    /// A folder that asks to be translated names a `.directory` file, not a string:
    /// the display name is that file's `[Desktop Entry] Name`, looked up under
    /// `desktop-directories/` with the **user** data dir first and the first file
    /// found winning outright (`shell-app-cache.c:112-140`) — so a user override
    /// shadows the system file even when its own `Name` is missing.
    #[test]
    fn a_translated_folder_name_comes_from_the_first_directory_file_found() {
        let root = std::env::temp_dir().join(format!("gsrs-folder-name-{}", std::process::id()));
        let user = root.join("user/desktop-directories");
        let system = root.join("system/desktop-directories");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(
            system.join("X-GNOME-Utilities.directory"),
            "[Desktop Entry]\nName=Utilities\nName[pt_BR]=Utilitários\n",
        )
        .unwrap();
        let dirs = [root.join("user"), root.join("system")];

        assert_eq!(
            translated_folder_name_in(&dirs, "X-GNOME-Utilities.directory").as_deref(),
            Some("Utilities")
        );
        assert_eq!(
            translated_folder_name_in(&dirs, "Nope.directory"),
            None,
            "a folder naming a file nobody ships keeps its raw name"
        );

        // The user file wins even though it carries no `Name` at all.
        std::fs::write(
            user.join("X-GNOME-Utilities.directory"),
            "[Desktop Entry]\nIcon=folder\n",
        )
        .unwrap();
        assert_eq!(
            translated_folder_name_in(&dirs, "X-GNOME-Utilities.directory"),
            None
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A profile that has never run a shell has an *empty* `folder-children`, and
    /// nothing but `_ensureDefaultFolders` (`appDisplay.js:1406-1431`) ever fills it —
    /// which is why the live seat showed no Utilities folder while a profile that had
    /// run stock gnome-shell did. It must run exactly once: the guard is no user value
    /// **and** an empty list, so a user who deletes every folder (leaving a user value
    /// of `[]`) does not get them all back on the next login.
    #[test]
    fn the_default_folders_seed_once_and_only_on_a_virgin_profile() {
        assert!(app_folders_need_seeding(false, &[]), "a virgin profile");
        assert!(
            !app_folders_need_seeding(true, &[]),
            "a user who emptied the list keeps it empty"
        );
        assert!(
            !app_folders_need_seeding(false, &["Utilities".to_owned()]),
            "a profile that already has folders is left alone"
        );

        // The table is GNOME's `DEFAULT_FOLDERS`, with the app lists filtered to what
        // is installed — every id in all four, so nothing distro-specific is dropped.
        let all = default_folders(|_| true);
        assert_eq!(
            all.iter().map(|f| f.id).collect::<Vec<_>>(),
            ["System", "Utilities", "YaST", "Pardus"]
        );
        let utilities = &all[1];
        assert_eq!(utilities.directory, "X-GNOME-Shell-Utilities.directory");
        assert!(utilities
            .apps
            .contains(&"org.gnome.Loupe.desktop".to_owned()));
        assert!(all[2].categories.contains(&"X-SuSE-YaST"));

        // On a machine with none of them installed the folders are still listed —
        // GNOME writes all four and lets the empty ones go undisplayed.
        let none = default_folders(|_| false);
        assert_eq!(none.len(), 4);
        assert!(none.iter().all(|f| f.apps.is_empty()));
    }

    /// The seed itself, against a memory backend: `folder-children` gains the four ids
    /// and each folder's own store gets `name` + `translate` (+ `categories`/`apps`),
    /// exactly the keys `_ensureDefaultFolders` writes (`appDisplay.js:1421-1429`).
    /// A second call must be a no-op — the first one left a user value behind.
    ///
    /// The memory backend is load-bearing, not decoration: the per-folder stores are
    /// relocatable, and opening them on the *default* backend would make this test
    /// overwrite the folders of whoever ran it.
    #[test]
    fn seeding_writes_every_default_folders_own_store() {
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return;
        };
        let (Some(parent_schema), Some(folder_schema)) = (
            source.lookup("org.gnome.desktop.app-folders", true),
            source.lookup(APP_FOLDER_SCHEMA, true),
        ) else {
            return; // schemas not installed
        };

        let backend = gio::memory_settings_backend_new();
        let parent = gio::Settings::new_full(&parent_schema, Some(&backend), None);
        let folders = default_folders(|id| id == "org.gnome.Loupe.desktop");

        ensure_default_folders(&parent, &folders, Some(&backend));

        assert_eq!(
            strv(&parent, "folder-children"),
            ["System", "Utilities", "YaST", "Pardus"]
        );
        let open = |id: &str| {
            let path = format!("/org/gnome/desktop/app-folders/folders/{id}/");
            gio::Settings::new_full(&folder_schema, Some(&backend), Some(&path))
        };

        let utilities = open("Utilities");
        assert_eq!(
            utilities.string("name"),
            "X-GNOME-Shell-Utilities.directory"
        );
        assert!(
            utilities.boolean("translate"),
            "a seeded name is a .directory file, so it must be translated"
        );
        assert_eq!(strv(&utilities, "apps"), ["org.gnome.Loupe.desktop"]);

        let yast = open("YaST");
        assert_eq!(strv(&yast, "categories"), ["X-SuSE-YaST"]);
        assert!(
            strv(&yast, "apps").is_empty(),
            "a category folder gets no app list"
        );

        // Second run: the user value the first left behind closes the door.
        let parent2 = gio::Settings::new_full(&parent_schema, Some(&backend), None);
        parent2.set_strv("folder-children", ["Mine"]).unwrap();
        ensure_default_folders(&parent2, &folders, Some(&backend));
        assert_eq!(strv(&parent2, "folder-children"), ["Mine"]);
    }

    /// The watcher re-reads the whole model whenever any key in any watched store
    /// changes — including the many keys we do not model — so an unrelated write
    /// produces a model identical to the one already applied. Forwarding it made the
    /// main loop re-derive everything downstream for nothing, and one such write
    /// lands a few seconds into every session.
    #[test]
    fn only_a_model_that_actually_changed_is_forwarded() {
        let seen = RefCell::new(None);
        let settings = GnomeSettings::default();

        assert!(
            is_new_model(&seen, &settings),
            "the first model must always go through — nothing has been applied yet"
        );
        assert!(
            !is_new_model(&seen, &settings),
            "an unrelated key changed and the re-read produced the same model: \
             forwarding it re-derives the whole downstream for nothing"
        );

        let mut changed = settings.clone();
        changed.icon_theme = "Papirus".to_owned();
        assert!(
            is_new_model(&seen, &changed),
            "a real change must go through"
        );
        assert!(!is_new_model(&seen, &changed), "and only once");
    }

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

    /// `app-picker-layout` is `aa{sv}` whose values are *doubly* boxed — a variant
    /// holding `a{sv}` holding a variant holding an int32. Getting either level wrong
    /// writes a variant gsettings rejects (or that gnome-shell can't read), which is
    /// invisible from this side, so the write is pinned by feeding it back through the
    /// reader we already had.
    #[test]
    fn the_app_picker_layout_round_trips_through_its_own_reader() {
        let pages = vec![
            vec!["a.desktop".to_owned(), "b.desktop".to_owned()],
            vec!["c.desktop".to_owned()],
        ];
        let value = build_app_picker_layout(&pages);
        assert_eq!(value.type_().as_str(), "aa{sv}");
        let read = read_app_picker_layout(&value);
        assert_eq!(read.get("a.desktop"), Some(&(0, 0)));
        assert_eq!(read.get("b.desktop"), Some(&(0, 1)));
        assert_eq!(read.get("c.desktop"), Some(&(1, 0)));
        assert_eq!(read.len(), 3);

        assert_eq!(build_app_picker_layout(&[]).type_().as_str(), "aa{sv}");
    }

    /// The base font size is the trailing number of a Pango description, whatever style
    /// words precede it — that is all `pango_font_description_from_string` reads as the
    /// size. A description with no size keeps the default rather than guessing.
    #[test]
    fn the_base_font_size_is_the_trailing_point_size() {
        assert_eq!(parse_font_size_pt("Cantarell 11"), Some(11.));
        assert_eq!(parse_font_size_pt("Cantarell 12"), Some(12.));
        assert_eq!(
            parse_font_size_pt("Source Sans Pro Semibold 10.5"),
            Some(10.5)
        );
        assert_eq!(parse_font_size_pt("Cantarell"), None);
        assert_eq!(parse_font_size_pt(""), None);
        assert_eq!(parse_font_size_pt("Cantarell 0"), None);
    }

    #[test]
    fn font_family_parsing() {
        assert_eq!(
            parse_font_family("Adwaita Sans 11").as_deref(),
            Some("Adwaita Sans")
        );
        assert_eq!(
            parse_font_family("Cantarell 12").as_deref(),
            Some("Cantarell")
        );
        // Style words come off, however they are spelled, and however many there are.
        assert_eq!(
            parse_font_family("Source Sans Pro Semibold 10.5").as_deref(),
            Some("Source Sans Pro")
        );
        assert_eq!(
            parse_font_family("Cantarell Bold Italic 11.5").as_deref(),
            Some("Cantarell")
        );
        assert_eq!(
            parse_font_family("Inter Semi-Bold 11").as_deref(),
            Some("Inter")
        );
        // A description need not carry a size — the size parser handles that half.
        assert_eq!(
            parse_font_family("Adwaita Sans").as_deref(),
            Some("Adwaita Sans")
        );
        // Nothing left to name: keep the default rather than ask for "".
        assert_eq!(parse_font_family(""), None);
        assert_eq!(parse_font_family("11"), None);
        assert_eq!(parse_font_family("Bold 11"), None);
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
                .filter(|kb| kb.action.gnome() == Some(action))
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
            accels_of(GnomeKeyAction::SwitchToWorkspace(1)),
            vec![Accel {
                trigger: AccelTrigger::Keysym(Keysym::Home),
                mods: AccelMods::SUPER,
            }],
            "switch-to-workspace-1 defaults to <Super>Home"
        );
        assert_eq!(
            accels_of(GnomeKeyAction::SwitchToWorkspace(2)),
            vec![],
            "switch-to-workspace-2 starts unbound — <Super>2 is switch-to-application-2"
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
        if source.lookup("org.gnome.mutter", true).is_none()
            || source
                .lookup("org.gnome.desktop.wm.keybindings", true)
                .is_none()
        {
            return;
        }

        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            // The REAL store set, on a private in-memory backend — so this exercises
            // `Stores::open` + `subscribe` + `read` as the compositor runs them, not a
            // two-store stand-in that can't notice a store we forgot to watch.
            let stores = Rc::new(Stores::open(SettingsStore::Memory));

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
                        .find(|kb| kb.action.gnome() == Some(GnomeKeyAction::Close))
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

    /// The a11y model reads every row from its own schema, and the Large Text row is a
    /// *factor*, not a flag (`ATIndicator._buildFontItem`,
    /// `js/ui/status/accessibility.js:118-129`). Memory backend throughout, so the
    /// user's real dconf is never touched.
    #[test]
    fn a11y_settings_read_every_row() {
        // The schemas come from the host system; skip where not installed.
        if !schema_available("org.gnome.desktop.a11y.keyboard", None)
            || !schema_available("org.gnome.desktop.interface", None)
        {
            return;
        }

        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            // The REAL store set + the REAL `read()` composition, on a private backend.
            let stores = Rc::new(Stores::open(SettingsStore::Memory));
            let write = |store: &str, f: &dyn Fn(&gio::Settings)| {
                f(stores.get(store).expect("store open on the memory backend"));
            };

            write("a11y-keyboard", &|s| {
                s.set_boolean("slowkeys-enable", true).unwrap()
            });
            write("interface", &|s| {
                s.set_double("text-scaling-factor", 1.25).unwrap()
            });

            let settings = stores.read();
            assert!(settings.a11y.get(A11yToggle::SlowKeys));
            assert!(!settings.a11y.get(A11yToggle::StickyKeys));
            assert!(
                settings.a11y.large_text(),
                "text-scaling-factor 1.25 reads as Large Text on"
            );
            assert!(settings.a11y.indicator_visible());

            // Exactly 1.0 is off — the reference tests `factor > 1.0`
            // (`accessibility.js:122`), not `!= 1.0`.
            write("interface", &|s| {
                s.set_double("text-scaling-factor", 1.0).unwrap()
            });
            let settings = stores.read();
            assert!(!settings.a11y.large_text());
            assert!(
                settings.a11y.indicator_visible(),
                "Slow Keys is still on, so the indicator stays up"
            );

            // With that last row off too, and no pin, it goes away.
            write("a11y-keyboard", &|s| {
                s.set_boolean("slowkeys-enable", false).unwrap()
            });
            assert!(!stores.read().a11y.indicator_visible());
        })
        .unwrap();
    }

    /// Drives the **real** settings stack on a private in-memory store:
    /// [`load_and_watch_gsettings_with`] spawns the production watcher thread,
    /// [`GnomeSettingsWriter`] performs the production write, and the re-read model arrives
    /// over the production calloop channel.
    ///
    /// This exists so the writer tests stop standing in for the wiring. A test that builds
    /// its own `Stores` and its own glib loop passes even when a store is missing from
    /// `Stores::all()` (→ no subscription → no delivery) or when the writer routes a key to
    /// a store `Stores::open` never opened — the two mistakes most likely to be made when
    /// adding a setting.
    struct RealWatcher {
        event_loop: calloop::EventLoop<'static, Option<GnomeSettings>>,
        latest: Option<GnomeSettings>,
        writer: GnomeSettingsWriter,
        initial: GnomeSettings,
    }

    impl RealWatcher {
        fn start() -> Self {
            let (initial, channel, writer) = load_and_watch_gsettings_with(SettingsStore::Memory);
            let event_loop = calloop::EventLoop::try_new().unwrap();
            event_loop
                .handle()
                .insert_source(channel, |event, _, latest: &mut Option<GnomeSettings>| {
                    if let calloop::channel::Event::Msg(settings) = event {
                        *latest = Some(settings);
                    }
                })
                .unwrap();
            Self {
                event_loop,
                latest: None,
                writer,
                initial,
            }
        }

        /// Pump the loop until `want` holds on a delivered model, then return it. The
        /// watcher is a real thread, so delivery is asynchronous.
        fn settle(&mut self, want: impl Fn(&GnomeSettings) -> bool, what: &str) -> &GnomeSettings {
            for _ in 0..100 {
                self.event_loop
                    .dispatch(Some(std::time::Duration::from_millis(50)), &mut self.latest)
                    .unwrap();
                if self.latest.as_ref().is_some_and(&want) {
                    return self.latest.as_ref().unwrap();
                }
            }
            panic!("the real watcher never delivered {what}: {:?}", self.latest);
        }
    }

    /// Whether a schema (and optionally a key) is installed on this host; these tests skip
    /// where GNOME's schemas aren't.
    fn schema_available(id: &str, key: Option<&str>) -> bool {
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return false;
        };
        let Some(schema) = source.lookup(id, true) else {
            return false;
        };
        key.is_none_or(|k| schema.has_key(k))
    }

    /// The a11y rows travel the real stack, and the one write that is not a boolean set is
    /// pinned: turning Large Text off **resets** `text-scaling-factor` rather than writing
    /// 1.0 (`accessibility.js:126-128`), so a system default other than 1.0 comes back
    /// instead of being pinned by us.
    #[test]
    fn the_real_watcher_delivers_a11y_writes() {
        if !schema_available("org.gnome.desktop.a11y.keyboard", Some("stickykeys-enable"))
            || !schema_available("org.gnome.desktop.interface", Some("text-scaling-factor"))
        {
            return;
        }

        let mut w = RealWatcher::start();
        assert!(
            !w.initial.a11y.indicator_visible(),
            "a pristine memory store has no a11y feature on"
        );

        // A plain boolean row.
        w.writer.set_a11y_toggle(A11yToggle::StickyKeys, true);
        let settings = w.settle(|s| s.a11y.get(A11yToggle::StickyKeys), "Sticky Keys on");
        assert!(
            settings.a11y.indicator_visible(),
            "one row on is enough to show the indicator (accessibility.js:96)"
        );

        // Large Text on writes the factor...
        w.writer.set_a11y_toggle(A11yToggle::LargeText, true);
        let settings = w.settle(|s| s.a11y.large_text(), "Large Text on");
        assert_eq!(settings.a11y.text_scaling_factor, DPI_FACTOR_LARGE);

        // ...and off RESETS it, so the schema default shows through.
        w.writer.set_a11y_toggle(A11yToggle::LargeText, false);
        let settings = w.settle(|s| !s.a11y.large_text(), "Large Text off");
        assert_eq!(
            settings.a11y.text_scaling_factor, 1.0,
            "Large Text off must reset the key, leaving the schema default"
        );
    }

    /// The run dialog's history round-trips the real stack.
    #[test]
    fn writer_persists_command_history() {
        if !schema_available("org.gnome.shell", Some("command-history")) {
            return;
        }
        let mut w = RealWatcher::start();
        w.writer.set_command_history(vec!["echo hi".to_owned()]);
        w.settle(
            |s| s.command_history == ["echo hi".to_owned()],
            "command-history",
        );
    }

    /// The dash's pinned apps round-trip the real stack.
    #[test]
    fn writer_persists_favorite_apps() {
        if !schema_available("org.gnome.shell", Some("favorite-apps")) {
            return;
        }
        let mut w = RealWatcher::start();
        w.writer
            .set_favorite_apps(vec!["org.gnome.Nautilus.desktop".to_owned()]);
        w.settle(
            |s| s.favorite_apps == ["org.gnome.Nautilus.desktop".to_owned()],
            "favorite-apps",
        );
    }
}
