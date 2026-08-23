// SPDX-License-Identifier: GPL-3.0-only
//
// Written for synoik in 2026.

//! Which display, everywhere in the shell.
//!
//! One type answers it for the live layout, the session store and `monitors.xml`. Two answers is
//! one too many: a workspace's home display and a saved window's display have to be the same
//! notion, or a session comes back onto a display its own workspace does not believe in. See
//! `docs/fork/multi-display.md` §1.

use smithay::output::Output;
use synoik_config::OutputName;

/// A display, in `monitors.xml`'s identity fields.
///
/// Deliberately the same four fields `<monitorspec>` carries, so that the deferred identity-only
/// matching lands everywhere at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputIdentity {
    pub connector: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

impl OutputIdentity {
    /// The identity of a live display.
    pub fn from_name(name: &OutputName) -> Self {
        Self {
            connector: name.connector.clone(),
            vendor: name.vendor.clone(),
            product: name.model.clone(),
            serial: name.serial.clone(),
        }
    }

    /// The identity of `output`.
    ///
    /// Panics if it carries no [`OutputName`]; every backend installs one when it creates the
    /// output, so an output without a name is a bug and not a case to paper over. Use
    /// [`Self::try_from_output`] where the output arrives from outside that guarantee.
    pub fn from_output(output: &Output) -> Self {
        Self::try_from_output(output).expect("an output always carries its OutputName")
    }

    /// The identity of `output`, or `None` if it carries no [`OutputName`].
    pub fn try_from_output(output: &Output) -> Option<Self> {
        output.user_data().get::<OutputName>().map(Self::from_name)
    }

    /// A display named by connector alone, with no EDID behind it.
    ///
    /// For a name that came from configuration rather than from hardware. It matches the connector
    /// and vetoes nothing, so it upgrades to a full identity the first time it meets its display
    /// (`Workspace::set_output`).
    pub fn from_connector(connector: impl Into<String>) -> Self {
        Self {
            connector: connector.into(),
            ..Self::default()
        }
    }

    /// Whether this names the same display as `other`.
    ///
    /// Connector-exact, with the EDID fields as a veto when both sides carry one: the same rule
    /// `monitors.xml` matching uses today. Matching a display across a *renamed* connector is the
    /// deferred half, and it is deferred here for the same reason — both stores should gain it
    /// together, or a session and its layout would disagree about which display is which.
    pub fn matches(&self, other: &Self) -> bool {
        if !self.connector.eq_ignore_ascii_case(&other.connector) {
            return false;
        }

        let agrees = |a: &Option<String>, b: &Option<String>| match (a, b) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            // One side did not record it. Absence is not a mismatch: an output with no EDID is
            // normal, and a record written before we read one must still match.
            _ => true,
        };

        agrees(&self.vendor, &other.vendor)
            && agrees(&self.product, &other.product)
            && agrees(&self.serial, &other.serial)
    }

    /// Whether this names the display `output` is.
    pub fn matches_output(&self, output: &Output) -> bool {
        Self::try_from_output(output).is_some_and(|id| self.matches(&id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_matches_on_its_connector_and_is_vetoed_by_a_differing_edid() {
        let saved = OutputIdentity {
            connector: "DP-2".into(),
            serial: Some("ABC123".into()),
            ..Default::default()
        };

        let live = |connector: &str, serial: Option<&str>| OutputIdentity {
            connector: connector.into(),
            serial: serial.map(str::to_owned),
            ..Default::default()
        };

        assert!(
            saved.matches(&live("dp-2", Some("abc123"))),
            "case is not identity"
        );
        assert!(
            saved.matches(&live("DP-2", None)),
            "an output with no EDID is normal, and absence is not a mismatch"
        );
        assert!(
            !saved.matches(&live("DP-2", Some("XYZ789"))),
            "a different display on the same connector is a different display"
        );
        assert!(
            !saved.matches(&live("DP-1", Some("ABC123"))),
            "matching the same panel across a renamed connector is deliberately deferred"
        );
    }
}
