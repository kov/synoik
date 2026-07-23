//! Reading GNOME/mutter's display-configuration store, `~/.config/monitors.xml`.
//!
//! This is **GNOME's way** of persisting per-monitor scale / transform / mode / layout: when the
//! user changes a display setting in Settings (or our quick-settings) with the "persistent" method,
//! mutter writes the chosen `<logicalmonitor>`s here keyed by a `<monitorspec>`, and restores them
//! on every login. niri's own KDL `output {}` config is *niri's* way; per the fork tenet
//! (`CLAUDE.md`) GNOME's store wins, so this file is consulted before the DPI guess and before the
//! KDL scale (see `State::reload_output_config`).
//!
//! We read the v2 format (`monitors version="2"`) that mutter 50.1 writes; the schema and matching
//! semantics follow `~/Projects/mutter/src/backends/meta-monitor-config-store.c`. Writing it back
//! lives with the `Mutter/DisplayConfig` `ApplyMonitorsConfig` handler.

use std::path::PathBuf;

use niri_config::OutputName;
use smithay::utils::Transform;

/// One saved logical-monitor setting for a single physical monitor, flattened out of the
/// `<logicalmonitor>`/`<monitor>` nesting. We keep only what we apply today (scale + transform);
/// position/mode/primary are parsed-but-ignored for now (single-monitor is the common case and the
/// preferred mode already matches — see the module TODO).
#[derive(Debug, Clone)]
pub struct MonitorSetting {
    /// EDID/DRM connector, e.g. `Virtual-1` — the primary match key.
    pub connector: String,
    /// `<product>` (≈ niri's `OutputName.model`), used to corroborate the connector match.
    pub product: Option<String>,
    /// `<serial>` (≈ niri's `OutputName.serial`), likewise corroborating.
    pub serial: Option<String>,
    pub scale: f64,
    pub transform: Transform,
}

impl MonitorSetting {
    /// Whether this saved setting is for `name`. mutter keys on the full `<monitorspec>`
    /// (connector, vendor, product, serial), but its `<vendor>` is the 3-letter EDID PNP id
    /// (`RHT`) whereas niri's `OutputName.make` is the decoded manufacturer (`Red Hat, Inc.`), so
    /// vendor never compares equal — we match on the connector and, when both sides carry them,
    /// corroborate with product/serial (which do line up: `krun-display`, `0x00000001`).
    fn matches(&self, name: &OutputName) -> bool {
        if self.connector != name.connector {
            return false;
        }
        let ok = |saved: &Option<String>, have: &Option<String>| match (saved, have) {
            (Some(a), Some(b)) => a == b,
            // If either side is missing the field, don't let it veto a connector match.
            _ => true,
        };
        ok(&self.product, &name.model) && ok(&self.serial, &name.serial)
    }
}

/// The parsed contents of `monitors.xml`: every saved per-monitor setting, flattened across all
/// `<configuration>`s. Lookup returns the first matching entry (see [`Self::setting_for`]).
#[derive(Debug, Clone, Default)]
pub struct MonitorsConfig {
    settings: Vec<MonitorSetting>,
}

impl MonitorsConfig {
    /// The path GNOME uses: `$XDG_CONFIG_HOME/monitors.xml`, else `~/.config/monitors.xml`.
    pub fn path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(dir).join("monitors.xml"));
        }
        std::env::var_os("HOME")
            .filter(|s| !s.is_empty())
            .map(|home| PathBuf::from(home).join(".config").join("monitors.xml"))
    }

    /// Load and parse the store, or `None` if it's absent/unreadable/unparseable (all non-fatal:
    /// we simply fall back to the DPI guess, exactly as if the user never saved a config).
    pub fn load() -> Option<Self> {
        let path = Self::path()?;
        let xml = std::fs::read_to_string(&path).ok()?;
        match Self::parse(&xml) {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                tracing::warn!("ignoring unparseable {}: {err}", path.display());
                None
            }
        }
    }

    /// Parse the v2 `monitors.xml` document. Unknown elements/attributes are ignored (forward-
    /// compatible with mutter adding fields). A `<configuration>` whose `<logicalmonitor>`s we
    /// can't fully read is simply skipped.
    pub fn parse(xml: &str) -> Result<Self, roxmltree::Error> {
        let doc = roxmltree::Document::parse(xml)?;
        let root = doc.root_element();
        // We only understand version 2 (mutter 50.1). Other versions → empty (fall back to guess).
        if root.tag_name().name() != "monitors" || root.attribute("version") != Some("2") {
            return Ok(Self::default());
        }

        let mut settings = Vec::new();
        for logical in root
            .children()
            .filter(|c| c.has_tag_name("configuration"))
            .flat_map(|conf| conf.children())
            .filter(|c| c.has_tag_name("logicalmonitor"))
        {
            let text = |parent: roxmltree::Node, tag: &str| {
                parent
                    .children()
                    .find(|c| c.has_tag_name(tag))
                    .and_then(|c| c.text())
                    .map(str::trim)
                    .map(str::to_owned)
            };

            let scale = match text(logical, "scale").and_then(|s| s.parse::<f64>().ok()) {
                Some(s) if s > 0. => s,
                _ => continue, // a logicalmonitor with no usable scale is not useful to us
            };
            let transform = parse_transform(logical);

            for monitor in logical.children().filter(|c| c.has_tag_name("monitor")) {
                let Some(spec) = monitor.children().find(|c| c.has_tag_name("monitorspec")) else {
                    continue;
                };
                let Some(connector) = text(spec, "connector") else {
                    continue;
                };
                settings.push(MonitorSetting {
                    connector,
                    product: text(spec, "product"),
                    serial: text(spec, "serial"),
                    scale,
                    transform,
                });
            }
        }

        Ok(Self { settings })
    }

    /// The saved setting for `name`, if any (`None` → use the DPI guess). First match wins; when a
    /// monitor appears in several `<configuration>`s (e.g. a laptop panel in both its solo and
    /// docked layouts) mutter would pick the configuration matching the *whole* connected set — we
    /// don't yet track the set, so we take the first, which is correct for the single-monitor case.
    pub fn setting_for(&self, name: &OutputName) -> Option<&MonitorSetting> {
        self.settings.iter().find(|s| s.matches(name))
    }
}

/// Read a `<logicalmonitor>`'s `<transform>` (`<rotation>` normal/left/right/upside_down +
/// `<flipped>` yes/no), mapping to Smithay's flip-then-rotate `Transform`. Absent → `Normal`.
fn parse_transform(logical: roxmltree::Node) -> Transform {
    let Some(t) = logical.children().find(|c| c.has_tag_name("transform")) else {
        return Transform::Normal;
    };
    let child_text = |tag: &str| {
        t.children()
            .find(|c| c.has_tag_name(tag))
            .and_then(|c| c.text())
            .map(str::trim)
    };
    let flipped = child_text("flipped") == Some("yes");
    match (child_text("rotation").unwrap_or("normal"), flipped) {
        ("left", false) => Transform::_90,
        ("upside_down", false) => Transform::_180,
        ("right", false) => Transform::_270,
        ("normal", true) => Transform::Flipped,
        ("left", true) => Transform::Flipped90,
        ("upside_down", true) => Transform::Flipped180,
        ("right", true) => Transform::Flipped270,
        _ => Transform::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(connector: &str, model: Option<&str>, serial: Option<&str>) -> OutputName {
        OutputName {
            connector: connector.to_owned(),
            make: None,
            model: model.map(str::to_owned),
            serial: serial.map(str::to_owned),
        }
    }

    const KOV_XML: &str = r#"<monitors version="2">
  <configuration>
    <layoutmode>logical</layoutmode>
    <logicalmonitor>
      <x>0</x>
      <y>0</y>
      <scale>2</scale>
      <primary>yes</primary>
      <monitor>
        <monitorspec>
          <connector>Virtual-1</connector>
          <vendor>RHT</vendor>
          <product>krun-display</product>
          <serial>0x00000001</serial>
        </monitorspec>
        <mode>
          <width>3840</width>
          <height>2160</height>
          <rate>59.996</rate>
        </mode>
      </monitor>
    </logicalmonitor>
  </configuration>
</monitors>"#;

    #[test]
    fn parses_and_matches_real_monitors_xml() {
        let cfg = MonitorsConfig::parse(KOV_XML).unwrap();
        // Matches by connector; product/serial corroborate; the "RHT" vs full-make mismatch is
        // deliberately not consulted.
        let s = cfg
            .setting_for(&name("Virtual-1", Some("krun-display"), Some("0x00000001")))
            .expect("saved setting for Virtual-1");
        assert_eq!(s.scale, 2.0);
        assert_eq!(s.transform, Transform::Normal);
        // A connector match with no model/serial on our side still matches (fields don't veto).
        assert!(cfg.setting_for(&name("Virtual-1", None, None)).is_some());
        // A different connector does not match.
        assert!(cfg.setting_for(&name("DP-3", None, None)).is_none());
        // A mismatching serial vetoes.
        assert!(cfg
            .setting_for(&name("Virtual-1", Some("krun-display"), Some("nope")))
            .is_none());
    }

    #[test]
    fn wrong_version_is_ignored() {
        let xml = KOV_XML.replace(r#"version="2""#, r#"version="1""#);
        assert!(MonitorsConfig::parse(&xml).unwrap().settings.is_empty());
    }

    #[test]
    fn transform_rotation_and_flip() {
        let xml = KOV_XML.replace(
            "<scale>2</scale>",
            "<scale>1.5</scale>\n      <transform><rotation>left</rotation><flipped>no</flipped></transform>",
        );
        let cfg = MonitorsConfig::parse(&xml).unwrap();
        let s = cfg.setting_for(&name("Virtual-1", None, None)).unwrap();
        assert_eq!(s.scale, 1.5);
        assert_eq!(s.transform, Transform::_90);
    }

    #[test]
    fn missing_or_garbage_is_empty_not_panic() {
        assert!(MonitorsConfig::parse("not xml at all <<<").is_err());
        assert!(MonitorsConfig::parse("<monitors version=\"2\"></monitors>")
            .unwrap()
            .settings
            .is_empty());
    }
}
