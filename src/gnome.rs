//! Fork-owned GNOME desktop policy.
//!
//! This module holds the inspectable model of the GNOME *settings* and policy the
//! compositor honors, kept deliberately separate from niri's own TOML config and
//! from the per-frame render path (see `docs/fork/STRATEGY.md`). GNOME policy
//! state flows through here as one inspectable struct rather than being scattered
//! across the input/render code.

use gio::glib;
use gio::prelude::SettingsExt;
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
}

impl Default for GnomeSettings {
    fn default() -> Self {
        Self {
            overlay_keys: vec![Keysym::Super_L, Keysym::Super_R],
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
pub fn load_and_watch_gsettings() -> (GnomeSettings, calloop::channel::Channel<GnomeSettings>) {
    let (tx, rx) = calloop::channel::channel();
    let (init_tx, init_rx) = std::sync::mpsc::channel();

    std::thread::Builder::new()
        .name("gsettings-watch".to_owned())
        .spawn(move || {
            let ctx = glib::MainContext::new();
            ctx.with_thread_default(|| {
                let mutter = gsettings("org.gnome.mutter");

                // Subscribe before the initial read so no change can fall
                // between them; a racing change just re-arrives via `tx`.
                let mut initial = GnomeSettings::default();
                if let Some(mutter) = &mutter {
                    subscribe_mutter(mutter, move |settings| {
                        let _ = tx.send(settings);
                    });
                    initial.load_mutter(mutter);
                }
                let _ = init_tx.send(initial);

                if mutter.is_some() {
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
    (initial, rx)
}

/// Invoke `on_change` with a freshly-read model whenever any `org.gnome.mutter`
/// key changes. The subscription lives as long as `mutter` does.
fn subscribe_mutter(mutter: &gio::Settings, on_change: impl Fn(GnomeSettings) + 'static) {
    mutter.connect_changed(None, move |mutter, _key| {
        let mut settings = GnomeSettings::default();
        settings.load_mutter(mutter);
        on_change(settings);
    });
}

/// Open a [`gio::Settings`] for `schema_id`, or `None` if the schema isn't
/// installed (e.g. running outside a GNOME environment). Guarding with the schema
/// source avoids `gio::Settings::new`'s abort-on-missing-schema behavior.
fn gsettings(schema_id: &str) -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    source.lookup(schema_id, true)?;
    Some(gio::Settings::new(schema_id))
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

    /// The change subscription re-reads the model when a key is written. Uses a
    /// memory settings backend so nothing touches the user's real dconf, and a
    /// private main context standing in for the watcher thread's.
    #[test]
    fn mutter_change_subscription_delivers_updates() {
        use std::cell::RefCell;
        use std::rc::Rc;

        // The schema comes from the host system; skip where it's not installed.
        let Some(source) = gio::SettingsSchemaSource::default() else {
            return;
        };
        let Some(schema) = source.lookup("org.gnome.mutter", true) else {
            return;
        };

        let ctx = glib::MainContext::new();
        ctx.with_thread_default(|| {
            let backend = gio::memory_settings_backend_new();
            let mutter = gio::Settings::new_full(&schema, Some(&backend), None);

            let received = Rc::new(RefCell::new(Vec::new()));
            subscribe_mutter(&mutter, {
                let received = received.clone();
                move |settings| received.borrow_mut().push(settings)
            });

            mutter.set_string("overlay-key", "Menu").unwrap();
            while ctx.pending() {
                ctx.iteration(false);
            }

            let received = received.borrow();
            assert_eq!(
                received.last().map(|s| s.overlay_keys.clone()),
                Some(vec![Keysym::Menu]),
                "a write to overlay-key must deliver a re-read model"
            );
        })
        .unwrap();
    }
}
