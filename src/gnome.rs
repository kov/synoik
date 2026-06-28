//! Fork-owned GNOME desktop policy.
//!
//! This module holds the inspectable model of the GNOME *settings* and policy the
//! compositor honors, kept deliberately separate from niri's own TOML config and
//! from the per-frame render path (see `docs/fork/STRATEGY.md`). GNOME policy
//! state flows through here as one inspectable struct rather than being scattered
//! across the input/render code.

use smithay::input::keyboard::Keysym;

/// GNOME desktop settings the compositor honors, mirroring the relevant
/// `org.gnome.*` GSettings keys.
///
/// For now this holds compiled-in GNOME defaults. A later increment will ingest
/// live values from GNOME's settings backend (dconf / GSettings) and update this
/// model at runtime; detection code already reads through it, so that ingestion
/// won't need to touch the input path.
#[derive(Debug, Clone)]
pub struct GnomeSettings {
    /// `org.gnome.mutter overlay-key`: the key whose lone tap toggles the
    /// Activities overview. `None` disables the overlay key entirely. GNOME's
    /// default is `Super_L`.
    pub overlay_key: Option<Keysym>,
}

impl Default for GnomeSettings {
    fn default() -> Self {
        Self {
            overlay_key: Some(Keysym::Super_L),
        }
    }
}
