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
    /// Whether product AND serial both corroborate `name` (both sides present and equal). Used only
    /// to *disambiguate* several saved entries that share one connector — not as a veto, because
    /// the serial/product we persist (from `niri_ipc::Output`, EDID PNP id for vendor,
    /// connector-fallback serial) does not always match the representation the reader sees in
    /// `OutputName` (e.g. a headless output reports serial `1` but persists `headless-1`).
    /// mutter keys on the full `<monitorspec>`, but its `<vendor>` is the PNP id (`RHT`) while
    /// our `OutputName.make` is the decoded manufacturer, so vendor never compares equal and is
    /// never consulted.
    fn corroborates(&self, name: &OutputName) -> bool {
        let eq = |saved: &Option<String>, have: &Option<String>| matches!((saved, have), (Some(a), Some(b)) if a == b);
        // Either field agreeing is enough to disambiguate; we don't require both, since a given
        // monitors.xml may carry only one of them.
        eq(&self.product, &name.model) || eq(&self.serial, &name.serial)
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

    /// The saved setting for `name`, if any (`None` → use the DPI guess). The **connector** is the
    /// match key: unique per session and the one field the writer and reader represent identically.
    /// If several saved entries share the connector (a monitor saved in more than one layout, or a
    /// different physical panel previously on the same port) we prefer the one whose product+serial
    /// also corroborate, else fall back to the first. (Divergence from mutter's whole-connected-set
    /// matching, noted in the module docs — correct for the single-monitor case and self-correcting
    /// otherwise: the user just re-picks the scale, which re-persists.)
    pub fn setting_for(&self, name: &OutputName) -> Option<&MonitorSetting> {
        let mut by_connector = self
            .settings
            .iter()
            .filter(|s| s.connector == name.connector);
        let first = by_connector.next()?;
        // Fast path: a single entry for this connector — use it regardless of product/serial.
        let Some(second) = by_connector.next() else {
            return Some(first);
        };
        // Multiple: prefer a corroborating entry, else the first.
        [first, second]
            .into_iter()
            .chain(
                self.settings
                    .iter()
                    .filter(|s| s.connector == name.connector),
            )
            .find(|s| s.corroborates(name))
            .or(Some(first))
    }
}

/// One physical monitor of a logical-monitor group to be written to `monitors.xml`: its full
/// `<monitorspec>` plus the chosen `<mode>`.
pub struct WriteMonitor {
    pub connector: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub width: i32,
    pub height: i32,
    pub rate: f64,
}

/// One `<logicalmonitor>` to be written: position, scale, primary flag, transform (the
/// `Mutter/DisplayConfig` u32 encoding, 0..=7), and its monitor(s).
pub struct WriteLogicalMonitor {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub primary: bool,
    pub transform: u32,
    pub monitors: Vec<WriteMonitor>,
}

/// Serialize `logical_monitors` to the v2 `monitors.xml` document mutter reads/writes. Format
/// mirrors `meta_monitor_config_store_save` closely enough for both our reader and a real mutter to
/// restore it: `<monitors version="2">` → one `<configuration>` (logical layout mode) → a
/// `<logicalmonitor>` per entry. Scale is written bare (`2` not `2.0`) like mutter; `<transform>`
/// is emitted only when non-normal.
pub fn serialize(logical_monitors: &[WriteLogicalMonitor]) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn fmt_scale(s: f64) -> String {
        if s.fract() == 0. {
            format!("{}", s as i64)
        } else {
            // Trim to a stable, mutter-like representation.
            format!("{s}")
        }
    }

    let mut out = String::from("<monitors version=\"2\">\n  <configuration>\n");
    out.push_str("    <layoutmode>logical</layoutmode>\n");
    for lm in logical_monitors {
        out.push_str("    <logicalmonitor>\n");
        out.push_str(&format!("      <x>{}</x>\n", lm.x));
        out.push_str(&format!("      <y>{}</y>\n", lm.y));
        out.push_str(&format!("      <scale>{}</scale>\n", fmt_scale(lm.scale)));
        if lm.primary {
            out.push_str("      <primary>yes</primary>\n");
        }
        if let Some((rotation, flipped)) = transform_elements(lm.transform) {
            out.push_str(&format!(
                "      <transform><rotation>{rotation}</rotation><flipped>{}</flipped></transform>\n",
                if flipped { "yes" } else { "no" },
            ));
        }
        for m in &lm.monitors {
            out.push_str("      <monitor>\n        <monitorspec>\n");
            out.push_str(&format!(
                "          <connector>{}</connector>\n",
                esc(&m.connector)
            ));
            out.push_str(&format!("          <vendor>{}</vendor>\n", esc(&m.vendor)));
            out.push_str(&format!(
                "          <product>{}</product>\n",
                esc(&m.product)
            ));
            out.push_str(&format!("          <serial>{}</serial>\n", esc(&m.serial)));
            out.push_str("        </monitorspec>\n        <mode>\n");
            out.push_str(&format!("          <width>{}</width>\n", m.width));
            out.push_str(&format!("          <height>{}</height>\n", m.height));
            out.push_str(&format!("          <rate>{:.3}</rate>\n", m.rate));
            out.push_str("        </mode>\n      </monitor>\n");
        }
        out.push_str("    </logicalmonitor>\n");
    }
    out.push_str("  </configuration>\n</monitors>\n");
    out
}

/// Write `logical_monitors` to the `monitors.xml` path atomically (temp file + rename), creating
/// the config dir if needed. Returns the path written.
pub fn write(logical_monitors: &[WriteLogicalMonitor]) -> std::io::Result<PathBuf> {
    let path = MonitorsConfig::path()
        .ok_or_else(|| std::io::Error::other("no config dir (HOME/XDG_CONFIG_HOME unset)"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let xml = serialize(logical_monitors);
    let tmp = path.with_extension("xml.tmp");
    std::fs::write(&tmp, xml.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Map the `Mutter/DisplayConfig` transform u32 to `monitors.xml` `(rotation, flipped)`, or `None`
/// for the identity (normal, unflipped — mutter omits `<transform>` then).
fn transform_elements(transform: u32) -> Option<(&'static str, bool)> {
    match transform {
        0 => None,
        1 => Some(("left", false)),
        2 => Some(("upside_down", false)),
        3 => Some(("right", false)),
        4 => Some(("normal", true)),
        5 => Some(("left", true)),
        6 => Some(("upside_down", true)),
        7 => Some(("right", true)),
        _ => None,
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
        // A UNIQUE connector matches even if serial differs — the writer's and reader's serial
        // representations can disagree (headless persists `headless-1` but reports `1`); connector
        // is the reliable key. Product/serial only disambiguate multiple same-connector entries.
        assert!(cfg
            .setting_for(&name("Virtual-1", Some("krun-display"), Some("nope")))
            .is_some());
    }

    #[test]
    fn duplicate_connector_prefers_corroborating_entry() {
        // Two saved layouts for the same connector with different scales: the one whose serial
        // corroborates wins; a non-matching serial falls back to the first.
        let xml = r#"<monitors version="2">
  <configuration><logicalmonitor><scale>1</scale>
    <monitor><monitorspec><connector>DP-1</connector><serial>AAA</serial></monitorspec></monitor>
  </logicalmonitor></configuration>
  <configuration><logicalmonitor><scale>3</scale>
    <monitor><monitorspec><connector>DP-1</connector><serial>BBB</serial></monitorspec></monitor>
  </logicalmonitor></configuration>
</monitors>"#;
        let cfg = MonitorsConfig::parse(xml).unwrap();
        assert_eq!(
            cfg.setting_for(&name("DP-1", None, Some("BBB")))
                .unwrap()
                .scale,
            3.
        );
        assert_eq!(
            cfg.setting_for(&name("DP-1", None, Some("AAA")))
                .unwrap()
                .scale,
            1.
        );
        // No corroboration → first entry.
        assert_eq!(
            cfg.setting_for(&name("DP-1", None, Some("ZZZ")))
                .unwrap()
                .scale,
            1.
        );
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
    fn serialize_round_trips_through_parse() {
        let lms = vec![WriteLogicalMonitor {
            x: 0,
            y: 0,
            scale: 2.0,
            primary: true,
            transform: 1, // 90° / rotation=left
            monitors: vec![WriteMonitor {
                connector: "Virtual-1".into(),
                vendor: "RHT".into(),
                product: "krun-display".into(),
                serial: "0x00000001".into(),
                width: 3840,
                height: 2160,
                rate: 59.996,
            }],
        }];
        let xml = serialize(&lms);
        assert!(
            xml.contains("<scale>2</scale>"),
            "whole scale is written bare: {xml}"
        );
        let cfg = MonitorsConfig::parse(&xml).unwrap();
        let s = cfg
            .setting_for(&name("Virtual-1", Some("krun-display"), Some("0x00000001")))
            .expect("round-tripped setting");
        assert_eq!(s.scale, 2.0);
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
