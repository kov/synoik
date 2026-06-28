//! Fork-owned GNOME desktop policy.
//!
//! This module holds the inspectable model of the GNOME *settings* and policy the
//! compositor honors, kept deliberately separate from niri's own TOML config and
//! from the per-frame render path (see `docs/fork/STRATEGY.md`). GNOME policy
//! state flows through here as one inspectable struct rather than being scattered
//! across the input/render code.

use gio::prelude::SettingsExt;
use smithay::input::keyboard::{xkb, Keysym};

/// GNOME desktop settings the compositor honors, mirroring the relevant
/// `org.gnome.*` GSettings keys.
///
/// [`Default`] is GNOME's compiled-in defaults; [`GnomeSettings::from_gsettings`]
/// overlays the live values read from the user's GSettings/dconf backend — the
/// same store gnome-shell/mutter use. Detection code reads through this model, so
/// updating it (now at startup, later live) never needs to touch the input path.
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
    /// Read the settings we honor from the live GSettings/dconf backend, falling
    /// back to GNOME's defaults for anything missing.
    ///
    /// Safe on non-GNOME systems: if a schema isn't installed we keep the default
    /// rather than aborting. This reads once; live change notification is a
    /// follow-up (it needs a glib main context bridged into our calloop loop).
    // FIXME: subscribe to `changed::` signals so edits apply without a restart.
    pub fn from_gsettings() -> Self {
        let mut settings = Self::default();
        settings.load_mutter();
        settings
    }

    fn load_mutter(&mut self) {
        let Some(mutter) = gsettings("org.gnome.mutter") else {
            return;
        };

        let overlay_key = mutter.string("overlay-key");
        match parse_overlay_key(overlay_key.as_str()) {
            Ok(keys) => self.overlay_keys = keys,
            Err(name) => warn!("ignoring unrecognized org.gnome.mutter overlay-key {name:?}"),
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
}
