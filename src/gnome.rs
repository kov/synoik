//! Fork-owned GNOME desktop policy.
//!
//! This module holds the inspectable model of the GNOME *settings* and policy the
//! compositor honors, kept deliberately separate from niri's own TOML config and
//! from the per-frame render path (see `docs/fork/STRATEGY.md`). GNOME policy
//! state flows through here as one inspectable struct rather than being scattered
//! across the input/render code.

use std::rc::Rc;

use gio::glib;
use gio::prelude::{SettingsExt, SettingsExtManual};
use smithay::input::keyboard::{xkb, Keysym};

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
    /// `org.gnome.desktop.lockdown disable-command-line`: when set, the run
    /// dialog refuses to open (gnome-shell's `RunDialog.open`).
    pub disable_command_line: bool,
}

impl Default for GnomeSettings {
    fn default() -> Self {
        Self {
            overlay_keys: vec![Keysym::Super_L, Keysym::Super_R],
            keybindings: default_keybindings(),
            command_history: Vec::new(),
            disable_command_line: false,
        }
    }
}

impl GnomeSettings {
    fn load_mutter(&mut self, mutter: &gio::Settings) {
        let overlay_key = mutter.string("overlay-key");
        match parse_overlay_key(overlay_key.as_str()) {
            Ok(keys) => self.overlay_keys = keys,
            Err(name) => warn!("ignoring unrecognized org.gnome.mutter overlay-key {name:?}"),
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
    }

    fn load_lockdown(&mut self, lockdown: &gio::Settings) {
        if settings_has_key(lockdown, "disable-command-line") {
            self.disable_command_line = lockdown.boolean("disable-command-line");
        }
    }

    fn load_wm_keybindings(&mut self, wm: &gio::Settings) {
        self.keybindings = adopted_wm_keybindings()
            .into_iter()
            .map(|(key, action, defaults)| {
                // Guard against schema drift: a missing key keeps our built-in
                // default rather than aborting inside gio.
                let values = if settings_has_key(wm, &key) {
                    wm.strv(&key).iter().map(|s| s.to_string()).collect()
                } else {
                    warn!("org.gnome.desktop.wm.keybindings has no {key:?}; using our default");
                    defaults
                };
                GnomeKeybinding {
                    action,
                    accels: parse_accels(&key, values),
                }
            })
            .collect();
    }
}

fn settings_has_key(settings: &gio::Settings, key: &str) -> bool {
    settings
        .settings_schema()
        .is_some_and(|schema| schema.has_key(key))
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
}

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
    adopted_wm_keybindings()
        .into_iter()
        .map(|(key, action, defaults)| GnomeKeybinding {
            action,
            accels: parse_accels(&key, defaults),
        })
        .collect()
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
}

/// The GSettings stores feeding [`GnomeSettings`]. Any change to any of them
/// re-reads the whole model — settings churn is rare and the read is cheap,
/// and one code path means one behavior to test.
struct Stores {
    mutter: Option<gio::Settings>,
    wm_keybindings: Option<gio::Settings>,
    shell: Option<gio::Settings>,
    lockdown: Option<gio::Settings>,
}

impl Stores {
    /// Open every schema we honor; each is `None` where not installed (e.g.
    /// running outside a GNOME environment).
    fn open() -> Self {
        Self {
            mutter: gsettings("org.gnome.mutter"),
            wm_keybindings: gsettings("org.gnome.desktop.wm.keybindings"),
            shell: gsettings("org.gnome.shell"),
            lockdown: gsettings("org.gnome.desktop.lockdown"),
        }
    }

    fn any(&self) -> bool {
        self.all().next().is_some()
    }

    fn all(&self) -> impl Iterator<Item = &gio::Settings> {
        [
            &self.mutter,
            &self.wm_keybindings,
            &self.shell,
            &self.lockdown,
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
        if let Some(wm) = &self.wm_keybindings {
            settings.load_wm_keybindings(wm);
        }
        if let Some(shell) = &self.shell {
            settings.load_shell(shell);
        }
        if let Some(lockdown) = &self.lockdown {
            settings.load_lockdown(lockdown);
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
}

/// Open a [`gio::Settings`] for `schema_id`, or `None` if the schema isn't
/// installed (e.g. running outside a GNOME environment). Guarding with the schema
/// source avoids `gio::Settings::new`'s abort-on-missing-schema behavior.
fn gsettings(schema_id: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema_id, true)?;
    Some(gio::Settings::new(schema_id))
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
fn parse_accelerator(accel: &str) -> Result<Option<Accel>, ()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
                wm_keybindings: Some(gio::Settings::new_full(&wm_schema, Some(&backend), None)),
                shell: None,
                lockdown: None,
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
                        wm_keybindings: None,
                        shell: Some(shell),
                        lockdown: None,
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
}
