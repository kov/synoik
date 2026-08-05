// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The IBus client — where composed text comes from.
//!
//! **Why the compositor needs one at all.** GNOME routes *every* key through IBus, including
//! plain xkb layouts: `js/ui/status/keyboard.js:510-528` turns an `xkb` input source into a
//! synthetic engine name (`us+intl` → `xkb:us:intl:eng`) and calls `setEngine` on it
//! unconditionally, and `js/misc/inputMethod.js:331-336` gates `filter_key_event` only on
//! "have a context" and "have a source" — never on the source's type. Dead keys and Compose
//! are implemented by IBus's `ibus-keyboard` engine, not by the keymap. So a compositor that
//! advertises `zwp_text_input_v3` and skips IBus for xkb layouts has taken GTK's own compose
//! table away (`gtkimmodule.c` picks its backend off that global's existence) and put nothing
//! in its place — which is exactly the dead-key bug this module exists to fix.
//!
//! # The shape of the connection
//!
//! ibus-daemon runs **its own message bus** on a private AF_UNIX socket — not the session bus,
//! but a real bus all the same (it implements `org.freedesktop.DBus`, and `ListNames` on it
//! answers). That is the important difference from [`super::gdm`], whose channel is a bare peer
//! connection: here `Hello`, `RequestName` and `AddMatch` all work, so ordinary zbus proxies are
//! fine and none of gdm's p2p contortions are needed.
//!
//! The address is discovered the way libibus does it, in priority order:
//!
//! 1. `$IBUS_ADDRESS`, used verbatim;
//! 2. `$IBUS_ADDRESS_FILE`, a path to an address file;
//! 3. `$XDG_CONFIG_HOME/ibus/bus/<machine-id>-<host>-<display>`, defaulting to `~/.config`.
//!
//! The file is `KEY=value` lines with `#` comments, carrying `IBUS_ADDRESS` and
//! `IBUS_DAEMON_PID`. **The pid matters:** these files are not cleaned up, and a developer
//! machine accumulates them — this one has address files dating to 2021 sitting beside live
//! ones. Connecting to a stale socket path fails slowly and confusingly, so a file whose
//! daemon is gone is discarded before it is ever dialed.
//!
//! # Serialization
//!
//! Every `v` argument in IBus's API is an `IBusSerializable`: a struct whose first two members
//! are always `(s name, a{sv} attachments)`, followed by the type's own fields. `IBusText` is
//! `(sa{sv}sv)` — name, attachments, the string, then an `IBusAttrList`. We only need the
//! string, so [`ibus_text`] reaches past the header rather than modelling the whole hierarchy.

use std::path::PathBuf;
use std::time::Duration;

use zbus::zvariant::{OwnedObjectPath, Value};

/// The client name handed to `CreateInputContext`. gnome-shell passes `'gnome-shell'`
/// (`js/misc/inputMethod.js:78`); ours says who we are for the same reason — it shows up in
/// ibus logs and in engine-side heuristics.
const CLIENT_NAME: &str = "synoik";

/// `IBus.Capabilite` (verified against the installed typelib). We claim what gnome-shell
/// claims: `PREEDIT_TEXT | FOCUS` as a baseline (`inputMethod.js:57`), plus
/// `SURROUNDING_TEXT` once a client has actually given us surrounding text.
pub const CAP_PREEDIT_TEXT: u32 = 1;
pub const CAP_FOCUS: u32 = 8;
pub const CAP_SURROUNDING_TEXT: u32 = 32;
pub const CAP_OSK: u32 = 64;

/// `IBus.ModifierType.RELEASE_MASK` — set on the state word to mark a key *release*, since
/// `ProcessKeyEvent` has no press/release argument (`inputMethod.js:342-343`).
pub const RELEASE_MASK: u32 = 1 << 30;
/// `IBus.ModifierType.IGNORED_MASK`. IBus sets this on events it has handed back to us via
/// `ForwardKeyEvent`; feeding such an event in again is an infinite loop, so the filter
/// short-circuits on it (`inputMethod.js:338-339`). Not optional.
pub const IGNORED_MASK: u32 = 1 << 25;

/// `IBus.PreeditFocusMode` — what an engine wants done with a live preedit when focus leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreeditMode {
    /// Throw the preedit away.
    Clear,
    /// Commit it as-is.
    Commit,
}

impl PreeditMode {
    pub fn from_wire(mode: u32) -> Self {
        // CLEAR = 0, COMMIT = 1. An unknown mode is treated as Clear: dropping an unfinished
        // preedit loses a few keystrokes, committing a wrong one corrupts the user's text.
        if mode == 1 {
            Self::Commit
        } else {
            Self::Clear
        }
    }
}

/// What an engine tells us, normalized to plain data.
///
/// This is deliberately the whole vocabulary the compositor needs — the lookup table, auxiliary
/// text and property list all arrive on the *panel* interface instead, which is a separate
/// (unported) surface. gnome-shell subscribes to exactly this set on the input context
/// (`inputMethod.js:85-95`).
#[derive(Debug, Clone, PartialEq)]
pub enum ImEvent {
    /// Finished text to insert at the caret.
    Commit(String),
    /// The in-progress composition. `text` is `None` when the preedit is being cleared;
    /// `cursor` is in **characters**, not bytes.
    Preedit {
        text: Option<String>,
        cursor: u32,
        visible: bool,
        mode: PreeditMode,
    },
    ShowPreedit,
    HidePreedit,
    /// A key the engine synthesized, or handed back unconsumed. `keycode` is **evdev** here;
    /// callers add 8 to get back to xkb (`inputMethod.js:192`).
    ForwardKey {
        keyval: u32,
        keycode: u32,
        state: u32,
        press: bool,
    },
    /// Delete around the caret. `offset` is in characters and may be negative (before the
    /// caret); `n_chars` is a length, not an end.
    DeleteSurrounding {
        offset: i32,
        n_chars: u32,
    },
    /// The engine wants surrounding text it has not been given.
    RequireSurrounding,
}

/// A resolved IBus address, with the daemon pid that wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub address: String,
    /// `IBUS_DAEMON_PID`, when the file carried one. `None` for `$IBUS_ADDRESS`, which is a
    /// deliberate override and gets no liveness check.
    pub pid: Option<i32>,
}

/// Discover the daemon's address the way libibus does.
///
/// Returns `None` when there is nothing to connect to — no env override, no address file, or a
/// file whose daemon is gone.
pub fn discover_address() -> Option<Address> {
    if let Ok(address) = std::env::var("IBUS_ADDRESS") {
        if !address.is_empty() {
            return Some(Address { address, pid: None });
        }
    }

    let path = address_file_path()?;
    let contents = std::fs::read_to_string(&path).ok()?;
    let found = parse_address_file(&contents)?;

    // A stale file points at a socket that is gone. Dialing it fails slowly and blames the
    // wrong thing, so check the writer is still alive first. `kill(pid, 0)` is the cheap probe;
    // it can only answer for a process we could signal, which the session's own daemon is.
    if let Some(pid) = found.pid {
        if !pid_is_alive(pid) {
            return None;
        }
    }

    Some(found)
}

/// Where libibus looks for the address file:
/// `$IBUS_ADDRESS_FILE`, else `$XDG_CONFIG_HOME/ibus/bus/<machine-id>-<host>-<display>`.
fn address_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("IBUS_ADDRESS_FILE") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }

    let machine_id = std::fs::read_to_string("/etc/machine-id").ok()?;
    let machine_id = machine_id.trim();
    if machine_id.is_empty() {
        return None;
    }

    let config = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").ok()?).join(".config"),
    };

    let (host, display) = host_and_display();
    Some(
        config
            .join("ibus/bus")
            .join(format!("{machine_id}-{host}-{display}")),
    )
}

/// The `<host>-<display>` half of the address-file name.
///
/// **`WAYLAND_DISPLAY` wins over `DISPLAY`**, which is the opposite of the obvious guess and was
/// worth getting wrong once: a GNOME session runs Xwayland, so `DISPLAY=:0` is set *as well*,
/// and picking it selects a file the current daemon never wrote. Verified against the live
/// daemon on this machine — pid 2880, with both variables set, wrote `…-unix-wayland-1` while
/// `…-unix-0` was a corpse from 2023. Under X11 there is no `WAYLAND_DISPLAY` and the `DISPLAY`
/// branch is the only one, so ordering it this way costs nothing there.
///
/// The X form is `host:display.screen`; an empty host means a local socket, which libibus
/// spells `unix`.
fn host_and_display() -> (String, String) {
    if let Ok(display) = std::env::var("WAYLAND_DISPLAY") {
        if !display.is_empty() {
            return ("unix".to_owned(), display);
        }
    }

    if let Ok(display) = std::env::var("DISPLAY") {
        if !display.is_empty() {
            let (host, rest) = display.split_once(':').unwrap_or(("", display.as_str()));
            let host = if host.is_empty() { "unix" } else { host };
            // Strip the screen: ":0.1" is display 0.
            let number = rest.split('.').next().unwrap_or(rest);
            return (host.to_owned(), number.to_owned());
        }
    }

    ("unix".to_owned(), "0".to_owned())
}

/// Pull `IBUS_ADDRESS` (and the pid, if present) out of an address file.
///
/// The file is `KEY=value` lines with `#` comments. A missing or empty address is `None` —
/// an address file without an address is not a usable one.
fn parse_address_file(contents: &str) -> Option<Address> {
    let mut address = None;
    let mut pid = None;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            // Split on the *first* `=` only: the value itself contains `=` signs
            // ("unix:path=…,guid=…").
            "IBUS_ADDRESS" => address = Some(value.trim().to_owned()),
            "IBUS_DAEMON_PID" => pid = value.trim().parse().ok(),
            _ => {}
        }
    }

    let address = address.filter(|a| !a.is_empty())?;
    Some(Address { address, pid })
}

fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs the permission and existence checks without delivering
    // anything, so this cannot affect the target.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM means a live process we may not signal — alive, and the answer we want. Only
    // ESRCH means gone. (Cross-user this is routinely EPERM; treating it as dead would
    // discard a perfectly good address whenever the daemon runs as another user.)
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Decode the string out of an `IBusText` variant, `(sa{sv}sv)`.
///
/// Returns `None` for anything that is not shaped like one, rather than guessing — a
/// misdecoded commit would insert garbage into the user's document.
pub fn ibus_text(value: &Value<'_>) -> Option<String> {
    let Value::Structure(fields) = value else {
        return None;
    };
    let fields = fields.fields();
    // 0: type name, 1: attachments, 2: the text.
    let Some(Value::Str(name)) = fields.first() else {
        return None;
    };
    if name.as_str() != "IBusText" {
        return None;
    }
    match fields.get(2) {
        Some(Value::Str(text)) => Some(text.to_string()),
        _ => None,
    }
}

/// `org.freedesktop.IBus` — the bus object, for engine selection and context creation.
#[zbus::proxy(
    interface = "org.freedesktop.IBus",
    default_service = "org.freedesktop.IBus",
    default_path = "/org/freedesktop/IBus"
)]
pub trait IBus {
    fn create_input_context(&self, client_name: &str) -> zbus::Result<OwnedObjectPath>;

    /// gnome-shell gives this a 4 s timeout and falls back to `xkb:us::eng` on failure
    /// (`ibusManager.js:275-296`); everything else uses the GDBus default.
    fn set_global_engine(&self, engine_name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn global_engine_changed(&self, engine_name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn registry_changed(&self) -> zbus::Result<()>;

    /// A **property**, despite libibus exposing it as `list_engines_async` — the `ListEngines`
    /// method is deprecated in the daemon's own introspection.
    #[zbus(property)]
    fn engines(&self) -> zbus::Result<Vec<zbus::zvariant::OwnedValue>>;

    /// Write-only property, not a method (`ibusManager.js:319-343` debounces it by 30 s).
    #[zbus(property)]
    fn set_preload_engines(&self, engines: Vec<String>) -> zbus::Result<()>;
}

/// `org.freedesktop.IBus.InputContext` — one per focus target. Ours is created once and reused;
/// the *Wayland* focus is mirrored onto it with `FocusIn`/`FocusOut`.
#[zbus::proxy(
    interface = "org.freedesktop.IBus.InputContext",
    default_service = "org.freedesktop.IBus"
)]
pub trait InputContext {
    /// The one hot call: a D-Bus round trip **per keystroke**. The reply decides whether the
    /// key is swallowed or replayed to the client, so this must never block the compositor
    /// thread. `keycode` is evdev (xkb − 8).
    fn process_key_event(&self, keyval: u32, keycode: u32, state: u32) -> zbus::Result<bool>;

    /// Per-context engine selection — **which usually does not work**, and is here so the next
    /// person does not rediscover that.
    ///
    /// `use-global-engine` is on by default, and with it the daemon refuses this outright:
    /// `org.freedesktop.DBus.Error.Failed: Cannot set engines when use-global-engine is
    /// enabled.` So gnome-shell's use of the bus-wide `SetGlobalEngine` (`ibusManager.js:283`)
    /// is not a stylistic preference — it is the only call that lands on a stock daemon. Reach
    /// for this one only after `GetUseGlobalEngine` says otherwise.
    fn set_engine(&self, name: &str) -> zbus::Result<()>;

    fn focus_in(&self) -> zbus::Result<()>;
    fn focus_out(&self) -> zbus::Result<()>;
    fn reset(&self) -> zbus::Result<()>;
    fn set_capabilities(&self, caps: u32) -> zbus::Result<()>;
    fn set_cursor_location(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;
    fn set_surrounding_text(
        &self,
        text: &Value<'_>,
        cursor_pos: u32,
        anchor_pos: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn commit_text(&self, text: Value<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn update_preedit_text_with_mode(
        &self,
        text: Value<'_>,
        cursor_pos: u32,
        visible: bool,
        mode: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn show_preedit_text(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn hide_preedit_text(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn forward_key_event(&self, keyval: u32, keycode: u32, state: u32) -> zbus::Result<()>;

    /// **Not in the daemon's introspection XML**, but emitted all the same — the name only
    /// exists as a string inside `libibus`. Subscribing by name is the only way to see it.
    #[zbus(signal)]
    fn delete_surrounding_text(&self, offset: i32, n_chars: u32) -> zbus::Result<()>;

    /// Also undeclared and also emitted; see
    /// [`InputContextProxy::receive_delete_surrounding_text`].
    #[zbus(signal)]
    fn require_surrounding_text(&self) -> zbus::Result<()>;

    /// `(uu)` — a *property* whose type is a tuple, so the variant it is set with must be the
    /// struct, not two scalars.
    #[zbus(property)]
    fn set_content_type(&self, value: (u32, u32)) -> zbus::Result<()>;

    /// Tells IBus that *we* commit the preedit on focus-out and reset, so the engine must not
    /// do it too (`inputMethod.js:84`). Without it a focus change can double-insert.
    #[zbus(property)]
    fn set_client_commit_preedit(&self, value: (bool,)) -> zbus::Result<()>;
}

/// The engine name for an input source, as gnome-shell builds it
/// (`js/ui/status/keyboard.js:513-520`).
///
/// An `ibus`-type source *is* its engine name. An `xkb` one is turned into the synthetic
/// `xkb:<layout>:<variant>:<lang>` engine that `ibus-keyboard` registers — this is the step
/// that makes dead keys work on a plain layout, so it is not an optimization to skip.
///
/// GNOME resolves `lang` through libgnome-desktop's `GnomeXkbInfo`, which this fork does not
/// link; `eng` is its own fallback when the lookup yields nothing (`keyboard.js:515`).
pub fn engine_name(source_type: &str, id: &str, language: Option<&str>) -> String {
    if source_type == "ibus" {
        return id.to_owned();
    }
    let (layout, variant) = id.split_once('+').unwrap_or((id, ""));
    let lang = language.unwrap_or("eng");
    format!("xkb:{layout}:{variant}:{lang}")
}

/// Connect to the daemon and create our input context.
///
/// The two are deliberately one step: a connection with no context is not useful to anyone, and
/// the failure modes (no daemon, no address, refused context) all want the same handling —
/// carry on without an input method.
pub async fn connect() -> anyhow::Result<(
    zbus::Connection,
    IBusProxy<'static>,
    InputContextProxy<'static>,
)> {
    let address = discover_address()
        .ok_or_else(|| anyhow::anyhow!("no ibus address (daemon not running?)"))?;

    let conn = zbus::connection::Builder::address(address.address.as_str())?
        .build()
        .await?;

    let bus = IBusProxy::new(&conn).await?;
    let path = bus.create_input_context(CLIENT_NAME).await?;

    let context = InputContextProxy::builder(&conn)
        .path(path)?
        .build()
        .await?;

    context.set_client_commit_preedit((true,)).await?;
    context
        .set_capabilities(CAP_PREEDIT_TEXT | CAP_FOCUS)
        .await?;

    Ok((conn, bus, context))
}

/// The D-Bus timeout gnome-shell puts on `SetGlobalEngine` and nothing else
/// (`ibusManager.js:59-61`): long enough for an engine to start, short enough that a wedged
/// one does not freeze the keyboard indefinitely.
pub const ENGINE_ACTIVATION_TIMEOUT: Duration = Duration::from_millis(4000);

#[cfg(test)]
mod tests {
    use super::*;

    /// The real file this machine's daemon wrote, comments and all.
    const SAMPLE: &str = "\
# This file is created by ibus-daemon, please do not modify it.
# If the IBUS_ADDRESS environment variable is set, it will
# be used rather than this file.
IBUS_ADDRESS=unix:path=/home/kov/.cache/ibus/dbus-6TnYAUim,guid=ac2be0220d4441b48fa4d0ed6a7287d2
IBUS_DAEMON_PID=2880
";

    #[test]
    fn address_file_keeps_the_equals_signs_in_the_value() {
        // Splitting on every `=` instead of the first would truncate the address at
        // "unix:path" and produce a socket path that cannot be dialed.
        let found = parse_address_file(SAMPLE).unwrap();
        assert_eq!(
            found.address,
            "unix:path=/home/kov/.cache/ibus/dbus-6TnYAUim,guid=ac2be0220d4441b48fa4d0ed6a7287d2"
        );
        assert_eq!(found.pid, Some(2880));
    }

    #[test]
    fn address_file_without_an_address_is_not_usable() {
        assert_eq!(parse_address_file("# just a comment\n"), None);
        assert_eq!(
            parse_address_file("IBUS_ADDRESS=\nIBUS_DAEMON_PID=1\n"),
            None
        );
        // A pid we cannot parse is not a reason to discard an otherwise good address.
        let found = parse_address_file("IBUS_ADDRESS=unix:path=/x\nIBUS_DAEMON_PID=nonsense\n");
        assert_eq!(found.unwrap().pid, None);
    }

    #[test]
    fn display_name_matches_the_files_ibus_writes() {
        // Observed on this machine: `<machine-id>-unix-wayland-1` beside `<machine-id>-unix-0`.
        // A local X display has an empty host, which libibus spells "unix".
        let cases = [
            (Some(":0"), None, ("unix", "0")),
            (Some(":1.0"), None, ("unix", "1")),
            (Some("remote:0"), None, ("remote", "0")),
            (None, Some("wayland-1"), ("unix", "wayland-1")),
            (None, None, ("unix", "0")),
            // The regression this ordering exists for: a GNOME session has BOTH set, because
            // Xwayland is running. Preferring DISPLAY here picks a file the live daemon never
            // wrote, and discovery fails with "no address" on a machine where ibus is running.
            (Some(":0"), Some("wayland-1"), ("unix", "wayland-1")),
        ];
        for (display, wayland, (host, number)) in cases {
            let got = with_display_env(display, wayland, host_and_display);
            assert_eq!(got, (host.to_owned(), number.to_owned()), "{display:?}");
        }
    }

    /// Run `f` with `DISPLAY`/`WAYLAND_DISPLAY` set as given. Env mutation in a parallel test
    /// binary is a flake generator, so this is the *only* place either is touched and it is
    /// serialized behind a mutex — see the `dump_dir_from` note in CLAUDE.md.
    fn with_display_env<T>(
        display: Option<&str>,
        wayland: Option<&str>,
        f: impl FnOnce() -> T,
    ) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let old_display = std::env::var_os("DISPLAY");
        let old_wayland = std::env::var_os("WAYLAND_DISPLAY");
        let restore = |key: &str, value: Option<std::ffi::OsString>| match value {
            // SAFETY: serialized by LOCK, and no other test touches these two.
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        };

        restore("DISPLAY", display.map(Into::into));
        restore("WAYLAND_DISPLAY", wayland.map(Into::into));
        let out = f();
        restore("DISPLAY", old_display);
        restore("WAYLAND_DISPLAY", old_wayland);
        out
    }

    #[test]
    fn engine_name_matches_gnome_shells_synthesis() {
        // The whole point: a plain xkb layout still gets an IBus engine, which is what
        // implements its dead keys.
        assert_eq!(
            engine_name("xkb", "us+intl", Some("eng")),
            "xkb:us:intl:eng"
        );
        assert_eq!(engine_name("xkb", "br", Some("por")), "xkb:br::por");
        // No language lookup (we don't link GnomeXkbInfo) falls back to "eng".
        assert_eq!(engine_name("xkb", "us", None), "xkb:us::eng");
        // An ibus source is already an engine name and must be passed through untouched.
        assert_eq!(engine_name("ibus", "libpinyin", None), "libpinyin");
    }

    #[test]
    fn ibus_text_refuses_anything_it_does_not_recognize() {
        use std::collections::HashMap;

        use zbus::zvariant::{Structure, StructureBuilder};

        let attachments: HashMap<String, Value<'_>> = HashMap::new();
        let good: Structure<'_> = StructureBuilder::new()
            .add_field("IBusText".to_string())
            .add_field(attachments.clone())
            .add_field("héllo".to_string())
            .build()
            .unwrap();
        assert_eq!(ibus_text(&Value::from(good)).as_deref(), Some("héllo"));

        // A struct that is not an IBusText must not be read as one.
        let wrong: Structure<'_> = StructureBuilder::new()
            .add_field("IBusProperty".to_string())
            .add_field(attachments)
            .add_field("héllo".to_string())
            .build()
            .unwrap();
        assert_eq!(ibus_text(&Value::from(wrong)), None);
        assert_eq!(ibus_text(&Value::from("bare string")), None);
    }

    #[test]
    fn an_unknown_preedit_mode_clears_rather_than_commits() {
        // Committing a preedit we did not understand corrupts the user's text; dropping it
        // costs a few keystrokes.
        assert_eq!(PreeditMode::from_wire(0), PreeditMode::Clear);
        assert_eq!(PreeditMode::from_wire(1), PreeditMode::Commit);
        assert_eq!(PreeditMode::from_wire(99), PreeditMode::Clear);
    }
}
