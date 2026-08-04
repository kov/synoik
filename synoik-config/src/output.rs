// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use synoik_ipc::{ConfiguredMode, Transform};

use crate::gestures::HotCorners;
use crate::{Color, FloatOrInt, LayoutPart};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Outputs(pub Vec<Output>);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mode {
    pub custom: bool,
    pub mode: ConfiguredMode,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Modeline {
    /// The rate at which pixels are drawn in MHz.
    pub clock: f64,
    /// Horizontal active pixels.
    pub hdisplay: u16,
    /// Horizontal sync pulse start position in pixels.
    pub hsync_start: u16,
    /// Horizontal sync pulse end position in pixels.
    pub hsync_end: u16,
    /// Total horizontal number of pixels before resetting the horizontal drawing position to
    /// zero.
    pub htotal: u16,

    /// Vertical active pixels.
    pub vdisplay: u16,
    /// Vertical sync pulse start position in pixels.
    pub vsync_start: u16,
    /// Vertical sync pulse end position in pixels.
    pub vsync_end: u16,
    /// Total vertical number of pixels before resetting the vertical drawing position to zero.
    pub vtotal: u16,
    /// Horizontal sync polarity: "+hsync" or "-hsync".
    pub hsync_polarity: synoik_ipc::HSyncPolarity,
    /// Vertical sync polarity: "+vsync" or "-vsync".
    pub vsync_polarity: synoik_ipc::VSyncPolarity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub off: bool,
    pub name: String,
    pub scale: Option<FloatOrInt<0, 10>>,
    pub transform: Transform,
    pub position: Option<Position>,
    pub max_bpc: Option<MaxBpc>,
    pub mode: Option<Mode>,
    pub modeline: Option<Modeline>,
    pub variable_refresh_rate: Option<Vrr>,
    pub focus_at_startup: bool,
    // Deprecated; use layout.background_color.
    pub background_color: Option<Color>,
    pub backdrop_color: Option<Color>,
    pub hot_corners: Option<HotCorners>,
    pub layout: Option<LayoutPart>,
}

impl Output {
    pub fn is_vrr_always_on(&self) -> bool {
        self.variable_refresh_rate == Some(Vrr { on_demand: false })
    }

    pub fn is_vrr_on_demand(&self) -> bool {
        self.variable_refresh_rate == Some(Vrr { on_demand: true })
    }

    pub fn is_vrr_always_off(&self) -> bool {
        self.variable_refresh_rate.is_none()
    }
}

impl Default for Output {
    fn default() -> Self {
        Self {
            off: false,
            focus_at_startup: false,
            name: String::new(),
            scale: None,
            transform: Transform::Normal,
            position: None,
            max_bpc: None,
            mode: None,
            modeline: None,
            variable_refresh_rate: None,
            background_color: None,
            backdrop_color: None,
            hot_corners: None,
            layout: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutputName {
    pub connector: String,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaxBpc(pub synoik_ipc::MaxBpc);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Vrr {
    pub on_demand: bool,
}

impl FromIterator<Output> for Outputs {
    fn from_iter<T: IntoIterator<Item = Output>>(iter: T) -> Self {
        Self(Vec::from_iter(iter))
    }
}

impl Outputs {
    pub fn find(&self, name: &OutputName) -> Option<&Output> {
        self.0.iter().find(|o| name.matches(&o.name))
    }

    pub fn find_mut(&mut self, name: &OutputName) -> Option<&mut Output> {
        self.0.iter_mut().find(|o| name.matches(&o.name))
    }
}

impl OutputName {
    pub fn from_ipc_output(output: &synoik_ipc::Output) -> Self {
        Self {
            connector: output.name.clone(),
            make: (output.make != "Unknown").then(|| output.make.clone()),
            model: (output.model != "Unknown").then(|| output.model.clone()),
            serial: output.serial.clone(),
        }
    }

    /// Returns an output description matching what Smithay's `Output::new()` does.
    pub fn format_description(&self) -> String {
        format!(
            "{} - {} - {}",
            self.make.as_deref().unwrap_or("Unknown"),
            self.model.as_deref().unwrap_or("Unknown"),
            self.connector,
        )
    }

    /// Returns an output name that will match by make/model/serial or, if they are missing, by
    /// connector.
    pub fn format_make_model_serial_or_connector(&self) -> String {
        if self.make.is_none() && self.model.is_none() && self.serial.is_none() {
            self.connector.to_string()
        } else {
            self.format_make_model_serial()
        }
    }

    pub fn format_make_model_serial(&self) -> String {
        let make = self.make.as_deref().unwrap_or("Unknown");
        let model = self.model.as_deref().unwrap_or("Unknown");
        let serial = self.serial.as_deref().unwrap_or("Unknown");
        format!("{make} {model} {serial}")
    }

    pub fn matches(&self, target: &str) -> bool {
        // Match by connector.
        if target.eq_ignore_ascii_case(&self.connector) {
            return true;
        }

        // If no other fields are available, don't try to match by them.
        //
        // This is used by synoik msg output.
        if self.make.is_none() && self.model.is_none() && self.serial.is_none() {
            return false;
        }

        // Match by "make model serial" with Unknown if something is missing.
        let make = self.make.as_deref().unwrap_or("Unknown");
        let model = self.model.as_deref().unwrap_or("Unknown");
        let serial = self.serial.as_deref().unwrap_or("Unknown");

        let Some(target_make) = target.get(..make.len()) else {
            return false;
        };
        let rest = &target[make.len()..];
        if !target_make.eq_ignore_ascii_case(make) {
            return false;
        }
        if !rest.starts_with(' ') {
            return false;
        }
        let rest = &rest[1..];

        let Some(target_model) = rest.get(..model.len()) else {
            return false;
        };
        let rest = &rest[model.len()..];
        if !target_model.eq_ignore_ascii_case(model) {
            return false;
        }
        if !rest.starts_with(' ') {
            return false;
        }

        let rest = &rest[1..];
        if !rest.eq_ignore_ascii_case(serial) {
            return false;
        }

        true
    }

    // Similar in spirit to Ord, but I don't want to derive Eq to avoid mistakes (you should use
    // `Self::match`, not Eq).
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        let self_missing_mms = self.make.is_none() && self.model.is_none() && self.serial.is_none();
        let other_missing_mms =
            other.make.is_none() && other.model.is_none() && other.serial.is_none();

        match (self_missing_mms, other_missing_mms) {
            (true, true) => self.connector.cmp(&other.connector),
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => self
                .make
                .cmp(&other.make)
                .then_with(|| self.model.cmp(&other.model))
                .then_with(|| self.serial.cmp(&other.serial))
                .then_with(|| self.connector.cmp(&other.connector)),
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;

    use super::*;

    #[test]
    fn parse_mode() {
        assert_eq!(
            "2560x1600@165.004".parse::<ConfiguredMode>().unwrap(),
            ConfiguredMode {
                width: 2560,
                height: 1600,
                refresh: Some(165.004),
            },
        );

        assert_eq!(
            "1920x1080".parse::<ConfiguredMode>().unwrap(),
            ConfiguredMode {
                width: 1920,
                height: 1080,
                refresh: None,
            },
        );

        assert!("1920".parse::<ConfiguredMode>().is_err());
        assert!("1920x".parse::<ConfiguredMode>().is_err());
        assert!("1920x1080@".parse::<ConfiguredMode>().is_err());
        assert!("1920x1080@60Hz".parse::<ConfiguredMode>().is_err());
    }

    fn make_output_name(
        connector: &str,
        make: Option<&str>,
        model: Option<&str>,
        serial: Option<&str>,
    ) -> OutputName {
        OutputName {
            connector: connector.to_string(),
            make: make.map(|x| x.to_string()),
            model: model.map(|x| x.to_string()),
            serial: serial.map(|x| x.to_string()),
        }
    }

    #[test]
    fn test_output_name_match() {
        fn check(
            target: &str,
            connector: &str,
            make: Option<&str>,
            model: Option<&str>,
            serial: Option<&str>,
        ) -> bool {
            let name = make_output_name(connector, make, model, serial);
            name.matches(target)
        }

        assert!(check("dp-2", "DP-2", None, None, None));
        assert!(!check("dp-1", "DP-2", None, None, None));
        assert!(check("dp-2", "DP-2", Some("a"), Some("b"), Some("c")));
        assert!(check(
            "some company some monitor 1234",
            "DP-2",
            Some("Some Company"),
            Some("Some Monitor"),
            Some("1234")
        ));
        assert!(!check(
            "some other company some monitor 1234",
            "DP-2",
            Some("Some Company"),
            Some("Some Monitor"),
            Some("1234")
        ));
        assert!(!check(
            "make model serial ",
            "DP-2",
            Some("make"),
            Some("model"),
            Some("serial")
        ));
        assert!(check(
            "make  serial",
            "DP-2",
            Some("make"),
            Some(""),
            Some("serial")
        ));
        assert!(check(
            "make model unknown",
            "DP-2",
            Some("Make"),
            Some("Model"),
            None
        ));
        assert!(check(
            "unknown unknown serial",
            "DP-2",
            None,
            None,
            Some("Serial")
        ));
        assert!(!check("unknown unknown unknown", "DP-2", None, None, None));
    }

    #[test]
    fn test_output_name_sorting() {
        let mut names = vec![
            make_output_name("DP-2", None, None, None),
            make_output_name("DP-1", None, None, None),
            make_output_name("DP-3", Some("B"), Some("A"), Some("A")),
            make_output_name("DP-3", Some("A"), Some("B"), Some("A")),
            make_output_name("DP-3", Some("A"), Some("A"), Some("B")),
            make_output_name("DP-3", None, Some("A"), Some("A")),
            make_output_name("DP-3", Some("A"), None, Some("A")),
            make_output_name("DP-3", Some("A"), Some("A"), None),
            make_output_name("DP-5", Some("A"), Some("A"), Some("A")),
            make_output_name("DP-4", Some("A"), Some("A"), Some("A")),
        ];
        names.sort_by(|a, b| a.compare(b));
        let names = names
            .into_iter()
            .map(|name| {
                format!(
                    "{} | {}",
                    name.format_make_model_serial_or_connector(),
                    name.connector,
                )
            })
            .collect::<Vec<_>>();
        assert_debug_snapshot!(
            names,
            @r#"
        [
            "Unknown A A | DP-3",
            "A Unknown A | DP-3",
            "A A Unknown | DP-3",
            "A A A | DP-4",
            "A A A | DP-5",
            "A A B | DP-3",
            "A B A | DP-3",
            "B A A | DP-3",
            "DP-1 | DP-1",
            "DP-2 | DP-2",
        ]
        "#
        );
    }
}
