// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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

use std::collections::BTreeSet;
use std::path::PathBuf;

use smithay::utils::Transform;
use synoik_config::OutputName;

/// One saved logical-monitor setting for a single physical monitor, flattened out of the
/// `<logicalmonitor>`/`<monitor>` nesting. We keep what we apply today (scale + transform) plus
/// the `<mode>` the setting was saved *for*; position/primary are parsed-but-ignored for now
/// (single-monitor is the common case and the preferred mode already matches — see the module
/// TODO).
#[derive(Debug, Clone)]
pub struct MonitorSetting {
    /// EDID/DRM connector, e.g. `Virtual-1` — the primary match key.
    pub connector: String,
    /// `<product>` (≈ niri's `OutputName.model`), used to corroborate the connector match.
    pub product: Option<String>,
    /// `<serial>` (≈ niri's `OutputName.serial`), likewise corroborating.
    pub serial: Option<String>,
    /// The `<mode>` this setting was saved for. In mutter a stored config is only applicable if
    /// this mode can actually be assigned on the monitor (`meta-monitor-config-manager.c:327`
    /// fails with "Invalid mode" otherwise, and `ensure_configured` then falls through to the
    /// guessed default) — a saved scale is a judgement about *that mode's* pixel density, not
    /// about the connector forever. We approximate with "matches the current mode" since we
    /// don't set modes from the store. `None` (absent in the XML) applies unconditionally.
    pub mode: Option<SavedMode>,
    pub scale: f64,
    pub transform: Transform,
}

/// The `<mode>` element of a saved `<monitor>`: what the monitor was running when the setting was
/// persisted.
#[derive(Debug, Clone, Copy)]
pub struct SavedMode {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in Hz; mutter compares rates with a 0.001 tolerance
    /// (`meta-monitor.c` `MAXIMUM_REFRESH_RATE_DIFF`).
    pub rate: f64,
}

impl SavedMode {
    /// Whether this saved mode is the given current mode, with mutter's refresh-rate tolerance.
    fn matches(&self, current: smithay::output::Mode) -> bool {
        self.width == current.size.w
            && self.height == current.size.h
            && (self.rate - f64::from(current.refresh) / 1000.).abs() < 0.001
    }
}

impl MonitorSetting {
    /// Whether product AND serial both corroborate `name` (both sides present and equal). Used only
    /// to *disambiguate* several saved entries that share one connector — not as a veto, because
    /// the serial/product we persist (from `synoik_ipc::Output`, EDID PNP id for vendor,
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

#[cfg(test)]
thread_local! {
    /// Test-only override for [`MonitorsConfig::path`]: the whole test harness runs on the test's
    /// own thread, so this lets a test point the store at a private file instead of the
    /// developer's real `~/.config/monitors.xml` — without the process-global races that
    /// `std::env::set_var` would inflict on parallel tests.
    pub static TEST_PATH: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

impl MonitorsConfig {
    /// The path GNOME uses: `$XDG_CONFIG_HOME/monitors.xml`, else `~/.config/monitors.xml`.
    pub fn path() -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(path) = TEST_PATH.with(|p| p.borrow().clone()) {
            return Some(path);
        }
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
                let mode = monitor
                    .children()
                    .find(|c| c.has_tag_name("mode"))
                    .and_then(|m| {
                        let num = |tag: &str| text(m, tag)?.parse::<f64>().ok();
                        Some(SavedMode {
                            width: num("width")? as i32,
                            height: num("height")? as i32,
                            rate: num("rate")?,
                        })
                    });
                settings.push(MonitorSetting {
                    connector,
                    product: text(spec, "product"),
                    serial: text(spec, "serial"),
                    mode,
                    scale,
                    transform,
                });
            }
        }

        Ok(Self { settings })
    }

    /// The saved setting for `name` at `current_mode`, if any (`None` → use the DPI guess). The
    /// **connector** is the match key: unique per session and the one field the writer and reader
    /// represent identically. A setting whose stored `<mode>` differs from the current mode is
    /// **not applicable** — the same connector at a different mode is effectively a different
    /// display (the krun window moving between the internal and an external screen changes the
    /// mode under a fixed `Virtual-1`), and mutter likewise rejects a stored config whose mode
    /// can't be assigned, falling back to the guess (`meta-monitor-manager.c`
    /// `ensure_configured`). If several applicable entries share the connector (a monitor saved
    /// in more than one layout, or a different physical panel previously on the same port) we
    /// prefer the one whose product+serial also corroborate, else fall back to the first.
    /// (Divergence from mutter's whole-connected-set matching, noted in the module docs — correct
    /// for the single-monitor case and self-correcting otherwise: the user just re-picks the
    /// scale, which re-persists.)
    pub fn setting_for(
        &self,
        name: &OutputName,
        current_mode: Option<smithay::output::Mode>,
    ) -> Option<&MonitorSetting> {
        let applicable = |s: &&MonitorSetting| {
            s.connector == name.connector
                && match (s.mode, current_mode) {
                    (Some(saved), Some(current)) => saved.matches(current),
                    // No stored mode (foreign/hand-edited XML) or no current mode: don't veto.
                    _ => true,
                }
        };
        let mut by_connector = self.settings.iter().filter(applicable);
        let first = by_connector.next()?;
        // Fast path: a single entry for this connector — use it regardless of product/serial.
        if by_connector.next().is_none() {
            return Some(first);
        }
        // Multiple: prefer a corroborating entry, else the first.
        self.settings
            .iter()
            .filter(applicable)
            .find(|s| s.corroborates(name))
            .or(Some(first))
    }

    /// Every mode saved for `name`'s connector, corroborating entries first.
    ///
    /// This is the *un*-gated counterpart to [`Self::setting_for`], and the two are a pair: the
    /// scale a setting carries is a judgement about the mode it was saved for, so restoring the
    /// scale means restoring the mode. mutter restores both together — a stored config assigns
    /// each monitor its `<mode>` (`meta-monitor-config-manager.c` `meta_monitor_config_new` →
    /// "Invalid mode" if it can't be assigned) — and without this a saved 1920x1200@125% never
    /// comes back: the connector lights up at its *preferred* mode and `setting_for`'s gate then
    /// rejects the entry.
    ///
    /// Several entries can share a connector (the same port having driven different panels), so
    /// the caller takes the first one the hardware actually advertises.
    pub fn saved_modes_for<'a>(
        &'a self,
        name: &'a OutputName,
    ) -> impl Iterator<Item = SavedMode> + 'a {
        let for_connector = |s: &&MonitorSetting| s.connector == name.connector;
        let corroborating = self
            .settings
            .iter()
            .filter(for_connector)
            .filter(|s| s.corroborates(name));
        let rest = self
            .settings
            .iter()
            .filter(for_connector)
            .filter(|s| !s.corroborates(name));
        corroborating.chain(rest).filter_map(|s| s.mode)
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

/// One monitor's identity as the store keys on it: connector + product + serial, case-folded.
///
/// mutter keys a stored configuration on the *set* of `MetaMonitorSpec`s it covers
/// (`MetaMonitorsConfigKey`, `meta-monitor-config-manager.c`; specs compared by
/// `meta_monitor_spec_equals` over connector/vendor/product/serial). We leave `<vendor>` out and
/// compare case-insensitively, because the same physical display is rendered differently by
/// different writers: mutter's `<vendor>` is the raw EDID manufacturer code (`LMN`) while ours is
/// the decoded make (`PNP(LMN)`), and mutter writes a fallback serial as `0x%08x` where we write
/// it uppercase. Keying on those bytes would file two stanzas for one display; connector + product
/// + serial is as discriminating in practice and survives a `monitors.xml` written by either.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SpecKey(String, String, String);

impl SpecKey {
    fn new(connector: &str, product: &str, serial: &str) -> Self {
        Self(
            connector.to_lowercase(),
            product.to_lowercase(),
            serial.to_lowercase(),
        )
    }

    /// The key of a `<configuration>` element: the set of its monitors' specs.
    fn set_of(configuration: roxmltree::Node) -> BTreeSet<Self> {
        let text = |parent: roxmltree::Node, tag: &str| {
            parent
                .children()
                .find(|c| c.has_tag_name(tag))
                .and_then(|c| c.text())
                .map(str::trim)
                .unwrap_or("")
                .to_owned()
        };
        configuration
            .children()
            .filter(|c| c.has_tag_name("logicalmonitor"))
            .flat_map(|lm| lm.children())
            .filter(|c| c.has_tag_name("monitor"))
            .filter_map(|m| m.children().find(|c| c.has_tag_name("monitorspec")))
            .map(|spec| {
                Self::new(
                    &text(spec, "connector"),
                    &text(spec, "product"),
                    &text(spec, "serial"),
                )
            })
            .collect()
    }

    /// The key of a configuration we are about to write.
    fn set_of_written(logical_monitors: &[WriteLogicalMonitor]) -> BTreeSet<Self> {
        logical_monitors
            .iter()
            .flat_map(|lm| &lm.monitors)
            .map(|m| Self::new(&m.connector, &m.product, &m.serial))
            .collect()
    }
}

/// Merge one configuration into an existing `monitors.xml` document, mutter-style: the entry for
/// *this* set of monitors is replaced, every other saved configuration is kept.
///
/// This is what makes per-display memory work. mutter's store is a hash table keyed on the
/// monitor-spec set (`meta-monitor-config-store.c`, `g_hash_table_replace`) and
/// `generate_config_xml` writes every entry, so configuring the laptop panel does not forget the
/// dock monitor — even when both arrive on one connector with different EDIDs, as they do in a VM
/// whose single virtual connector mirrors whichever host display the window sits on.
///
/// Configurations we keep are copied through as **raw source text**, not re-serialized: the reader
/// models only what it applies (scale, transform, mode), and a lossy round-trip would quietly eat
/// mutter's fields — full-precision rates, `<disabled>`, anything a newer mutter adds — on every
/// save. An `existing` document we can't parse, or one that isn't version 2, is discarded: we don't
/// understand its entries well enough to key them, so it cannot be merged into.
pub fn merge(existing: Option<&str>, logical_monitors: &[WriteLogicalMonitor]) -> String {
    let mut out = String::from("<monitors version=\"2\">\n");

    if let Some(xml) = existing {
        let new_key = SpecKey::set_of_written(logical_monitors);
        if let Ok(doc) = roxmltree::Document::parse(xml) {
            let root = doc.root_element();
            if root.tag_name().name() == "monitors" && root.attribute("version") == Some("2") {
                for conf in root.children().filter(|c| c.has_tag_name("configuration")) {
                    if SpecKey::set_of(conf) == new_key {
                        continue; // replaced by the configuration we're writing
                    }
                    out.push_str("  ");
                    out.push_str(xml[conf.range()].trim_end());
                    out.push('\n');
                }
            }
        }
    }

    out.push_str(&serialize_configuration(logical_monitors));
    out.push_str("</monitors>\n");
    out
}

/// Serialize `logical_monitors` to a whole v2 `monitors.xml` document, discarding anything already
/// stored. Prefer [`merge`] for a save; this is the from-scratch case (and the reader's fixture).
pub fn serialize(logical_monitors: &[WriteLogicalMonitor]) -> String {
    merge(None, logical_monitors)
}

/// Serialize one `<configuration>` element (indented two spaces, newline-terminated). Format
/// mirrors `meta_monitor_config_store_save` closely enough for both our reader and a real mutter to
/// restore it: `<configuration>` (logical layout mode) → a `<logicalmonitor>` per entry. Scale is
/// written bare (`2` not `2.0`) like mutter; `<transform>` is emitted only when non-normal.
fn serialize_configuration(logical_monitors: &[WriteLogicalMonitor]) -> String {
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

    let mut out = String::from("  <configuration>\n");
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
    out.push_str("  </configuration>\n");
    out
}

/// Write `logical_monitors` to the `monitors.xml` path atomically (temp file + rename), creating
/// the config dir if needed. Returns the path written.
///
/// The configuration is [`merge`]d into whatever is already stored, so saving settings for one
/// display keeps the other displays' saved settings.
pub fn write(logical_monitors: &[WriteLogicalMonitor]) -> std::io::Result<PathBuf> {
    let path = MonitorsConfig::path()
        .ok_or_else(|| std::io::Error::other("no config dir (HOME/XDG_CONFIG_HOME unset)"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let existing = std::fs::read_to_string(&path).ok();
    let xml = merge(existing.as_deref(), logical_monitors);
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

    fn mode(width: i32, height: i32, rate: f64) -> Option<smithay::output::Mode> {
        Some(smithay::output::Mode {
            size: (width, height).into(),
            refresh: (rate * 1000.).round() as i32,
        })
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
        let at_4k = mode(3840, 2160, 59.996);
        // Matches by connector; product/serial corroborate; the "RHT" vs full-make mismatch is
        // deliberately not consulted.
        let s = cfg
            .setting_for(
                &name("Virtual-1", Some("krun-display"), Some("0x00000001")),
                at_4k,
            )
            .expect("saved setting for Virtual-1");
        assert_eq!(s.scale, 2.0);
        assert_eq!(s.transform, Transform::Normal);
        // A connector match with no model/serial on our side still matches (fields don't veto).
        assert!(cfg
            .setting_for(&name("Virtual-1", None, None), at_4k)
            .is_some());
        // A different connector does not match.
        assert!(cfg.setting_for(&name("DP-3", None, None), at_4k).is_none());
        // A UNIQUE connector matches even if serial differs — the writer's and reader's serial
        // representations can disagree (headless persists `headless-1` but reports `1`); connector
        // is the reliable key. Product/serial only disambiguate multiple same-connector entries.
        assert!(cfg
            .setting_for(
                &name("Virtual-1", Some("krun-display"), Some("nope")),
                at_4k
            )
            .is_some());
    }

    #[test]
    fn saved_scale_is_pinned_to_its_mode() {
        // The overview-port S11 bug: the krun VM window moving to the laptop's internal screen
        // keeps the connector (Virtual-1) but changes the mode to 2048x1330 — the scale 2 saved
        // for the 4K mode must NOT apply there (mutter rejects a stored config whose mode can't
        // be assigned and falls back to the guess), else the desktop runs on a 1024x665 logical
        // canvas.
        let cfg = MonitorsConfig::parse(KOV_XML).unwrap();
        let internal = name("Virtual-1", Some("krun-display"), Some("0x00000001"));
        assert!(cfg
            .setting_for(&internal, mode(2048, 1330, 59.996))
            .is_none());
        // Same resolution at a clearly different refresh rate is a different mode too...
        assert!(cfg.setting_for(&internal, mode(3840, 2160, 30.0)).is_none());
        // ...but mutter's MAXIMUM_REFRESH_RATE_DIFF (0.001) tolerates representation noise: a
        // real mutter writes the full float ("59.996398925781250") while our modes carry integer
        // millihertz, so exact comparison would spuriously reject every mutter-written file.
        let mutter_precision = KOV_XML.replace("59.996", "59.996398925781250");
        let cfg_mutter = MonitorsConfig::parse(&mutter_precision).unwrap();
        assert!(cfg_mutter
            .setting_for(&internal, mode(3840, 2160, 59.996))
            .is_some());
        // An unknown current mode does not veto (we'd rather restore than guess).
        assert!(cfg.setting_for(&internal, None).is_some());
    }

    #[test]
    fn a_saved_mode_is_restorable_even_though_the_gate_rejects_it() {
        // The other half of `saved_scale_is_pinned_to_its_mode`: gating the scale on the mode only
        // restores anything if we also *set* the mode. At login the connector lights up at its
        // preferred mode, so `setting_for` says no — but `saved_modes_for` still offers the saved
        // mode, and once the backend has switched to it the gate matches and the scale comes back.
        // (Gustavo, 2026-07-28: saved 1920x1200 @ 125%, logged back in to 2048x1330 @ 225%.)
        let cfg = MonitorsConfig::parse(&KOV_XML.replace("3840", "1920").replace("2160", "1200"))
            .unwrap();
        let internal = name("Virtual-1", Some("krun-display"), Some("0x00000001"));
        let preferred = mode(2048, 1330, 59.996);

        assert!(cfg.setting_for(&internal, preferred).is_none());

        let saved: Vec<_> = cfg.saved_modes_for(&internal).collect();
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].width, saved[0].height), (1920, 1200));

        let restored = mode(saved[0].width, saved[0].height, saved[0].rate);
        assert_eq!(cfg.setting_for(&internal, restored).unwrap().scale, 2.0);
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
            cfg.setting_for(&name("DP-1", None, Some("BBB")), None)
                .unwrap()
                .scale,
            3.
        );
        assert_eq!(
            cfg.setting_for(&name("DP-1", None, Some("AAA")), None)
                .unwrap()
                .scale,
            1.
        );
        // No corroboration → first entry.
        assert_eq!(
            cfg.setting_for(&name("DP-1", None, Some("ZZZ")), None)
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
        let s = cfg
            .setting_for(&name("Virtual-1", None, None), None)
            .unwrap();
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
            .setting_for(
                &name("Virtual-1", Some("krun-display"), Some("0x00000001")),
                mode(3840, 2160, 59.996),
            )
            .expect("round-tripped setting");
        assert_eq!(s.scale, 2.0);
        assert_eq!(s.transform, Transform::_90);
    }

    /// One saved display on `Virtual-1`, mutter's own formatting: raw PNP `<vendor>`, lowercase
    /// fallback serial, full-precision rate, and a `<doublescan>` we don't model.
    fn mutter_written(product: &str, serial: &str, w: i32, h: i32, scale: &str) -> String {
        format!(
            r#"<monitors version="2">
  <configuration>
    <layoutmode>logical</layoutmode>
    <logicalmonitor>
      <x>0</x>
      <y>0</y>
      <scale>{scale}</scale>
      <primary>yes</primary>
      <monitor>
        <monitorspec>
          <connector>Virtual-1</connector>
          <vendor>LMN</vendor>
          <product>{product}</product>
          <serial>{serial}</serial>
        </monitorspec>
        <mode>
          <width>{w}</width>
          <height>{h}</height>
          <rate>59.996398925781250</rate>
          <doublescan>no</doublescan>
        </mode>
      </monitor>
    </logicalmonitor>
  </configuration>
</monitors>
"#
        )
    }

    fn write_lm(product: &str, serial: &str, w: i32, h: i32, scale: f64) -> WriteLogicalMonitor {
        WriteLogicalMonitor {
            x: 0,
            y: 0,
            scale,
            primary: true,
            transform: 0,
            monitors: vec![WriteMonitor {
                connector: "Virtual-1".into(),
                vendor: "PNP(LMN)".into(),
                product: product.into(),
                serial: serial.into(),
                width: w,
                height: h,
                rate: 59.996,
            }],
        }
    }

    #[test]
    fn saving_one_display_keeps_the_others() {
        // The limina report's finding 2: with one virtual connector whose EDID mirrors whichever
        // host display the VM's window sits on, saving a scale for the built-in panel used to
        // *replace* the external panel's stanza, so nothing could be remembered per display.
        let external = mutter_written("BenQ LCD", "0x6c42fae5", 2560, 1440, "1.25");
        let merged = merge(
            Some(&external),
            &[write_lm("Built-in", "0x31d7dd41", 3024, 1964, 1.75)],
        );

        let cfg = MonitorsConfig::parse(&merged).unwrap();
        assert_eq!(
            cfg.setting_for(
                &name("Virtual-1", Some("BenQ LCD"), Some("0x6c42fae5")),
                mode(2560, 1440, 59.996)
            )
            .unwrap()
            .scale,
            1.25,
            "the external panel's saved scale survives a save for the built-in: {merged}"
        );
        assert_eq!(
            cfg.setting_for(
                &name("Virtual-1", Some("Built-in"), Some("0x31d7dd41")),
                mode(3024, 1964, 59.996)
            )
            .unwrap()
            .scale,
            1.75
        );
        // Kept configurations are copied through verbatim, so fields we don't model — mutter's
        // full-precision rate, its `<doublescan>` — are not eaten by the round trip.
        assert!(
            merged.contains("<rate>59.996398925781250</rate>"),
            "{merged}"
        );
        assert!(merged.contains("<doublescan>no</doublescan>"), "{merged}");
    }

    #[test]
    fn re_saving_a_display_replaces_only_its_own_stanza() {
        let external = mutter_written("BenQ LCD", "0x6c42fae5", 2560, 1440, "1.25");
        let two = merge(
            Some(&external),
            &[write_lm("Built-in", "0x31d7dd41", 3024, 1964, 1.75)],
        );
        // Same display (identity and connector), new scale: one stanza replaced, one untouched.
        let again = merge(
            Some(&two),
            &[write_lm("Built-in", "0x31d7dd41", 3024, 1964, 2.0)],
        );
        assert_eq!(again.matches("<configuration>").count(), 2, "{again}");

        let cfg = MonitorsConfig::parse(&again).unwrap();
        assert_eq!(
            cfg.setting_for(
                &name("Virtual-1", Some("Built-in"), Some("0x31d7dd41")),
                mode(3024, 1964, 59.996)
            )
            .unwrap()
            .scale,
            2.0
        );
        assert_eq!(
            cfg.setting_for(
                &name("Virtual-1", Some("BenQ LCD"), Some("0x6c42fae5")),
                mode(2560, 1440, 59.996)
            )
            .unwrap()
            .scale,
            1.25
        );
    }

    #[test]
    fn a_mutter_written_stanza_for_the_same_display_is_replaced_not_duplicated() {
        // The store key deliberately ignores `<vendor>` and folds case: mutter writes `LMN` /
        // `0x6c42fae5` where we write `PNP(LMN)` / uppercase, and keying on those bytes would file
        // a second stanza for the one display the user is configuring.
        let mutter = mutter_written("BenQ LCD", "0x6c42fae5", 2560, 1440, "1.25");
        let ours = merge(
            Some(&mutter),
            &[write_lm("BenQ LCD", "0x6C42FAE5", 2560, 1440, 1.5)],
        );
        assert_eq!(ours.matches("<configuration>").count(), 1, "{ours}");
        assert_eq!(
            MonitorsConfig::parse(&ours)
                .unwrap()
                .setting_for(
                    &name("Virtual-1", Some("BenQ LCD"), Some("0x6C42FAE5")),
                    mode(2560, 1440, 59.996)
                )
                .unwrap()
                .scale,
            1.5
        );
    }

    /// The whole path `ApplyMonitorsConfig(PERSISTENT)` takes: two saves for two displays leave two
    /// stanzas on disk.
    #[test]
    fn write_merges_into_the_file_on_disk() {
        let path = std::env::temp_dir().join(format!(
            "synoik-test-monitors-merge-{}-{:?}.xml",
            std::process::id(),
            std::thread::current().id(),
        ));
        let _ = std::fs::remove_file(&path);
        TEST_PATH.with(|p| *p.borrow_mut() = Some(path.clone()));

        write(&[write_lm("BenQ LCD", "0x6C42FAE5", 2560, 1440, 1.25)]).unwrap();
        write(&[write_lm("Built-in", "0x31D7DD41", 3024, 1964, 1.75)]).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();

        TEST_PATH.with(|p| *p.borrow_mut() = None);
        let _ = std::fs::remove_file(&path);

        assert_eq!(on_disk.matches("<configuration>").count(), 2, "{on_disk}");
        let cfg = MonitorsConfig::parse(&on_disk).unwrap();
        for (product, serial, m, scale) in [
            ("BenQ LCD", "0x6C42FAE5", mode(2560, 1440, 59.996), 1.25),
            ("Built-in", "0x31D7DD41", mode(3024, 1964, 59.996), 1.75),
        ] {
            assert_eq!(
                cfg.setting_for(&name("Virtual-1", Some(product), Some(serial)), m)
                    .unwrap()
                    .scale,
                scale,
                "{product}: {on_disk}"
            );
        }
    }

    #[test]
    fn merging_into_an_unusable_document_replaces_it() {
        let new = [write_lm("Built-in", "0x31d7dd41", 3024, 1964, 1.75)];
        for existing in [
            "not xml at all <<<",
            &KOV_XML.replace(r#"version="2""#, r#"version="1""#),
        ] {
            let merged = merge(Some(existing), &new);
            assert_eq!(merged, serialize(&new), "existing: {existing}");
        }
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
