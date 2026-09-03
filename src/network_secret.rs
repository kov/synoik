// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! What the network secret dialog asks for, derived from the connection NetworkManager sent.
//!
//! A port of `NetworkSecretDialog._getContent` and its helpers
//! (`js/ui/components/networkAgent.js:158-419`), kept free of D-Bus and of rendering: the agent
//! ([`crate::dbus::network_agent`]) flattens NM's `a{sa{sv}}` into a [`ConnectionInfo`], this
//! decides the title, the message and the list of entries, and the dialog draws them. The same
//! function backs the conformance tests, which is why it takes plain data rather than a message.
//!
//! **A field's `value` is never a secret.** GNOME prefills the password entry from the connection
//! it was handed (`:214`, `:238`, …). We deliberately do not: the only prefill here is
//! non-secret identity text (a username, a service name, the connection's own name), so no
//! plaintext ever reaches the model, the UI or a `{:?}`. The cost is one divergence — when NM
//! re-asks with `REQUEST_NEW` the entry starts empty instead of holding the rejected password —
//! and `REQUEST_NEW` means that password was wrong, so there is little to preserve.
//!
//! VPN is out of scope, here and in the agent: upstream hands every VPN request to the plugin's
//! own auth binary (`VPNRequestHandler`, `:419-671`), a whole second protocol over a spawned
//! process. [`content`] returns `None` for it and the agent answers NM rather than hanging.

use std::collections::HashMap;

/// NM's `wep-key-type` (`NMWepKeyType`): which alphabet a static WEP key is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WepKeyType {
    /// `NM_WEP_KEY_TYPE_UNKNOWN` — validated as a key, upstream's own fallthrough.
    #[default]
    Unknown,
    /// `NM_WEP_KEY_TYPE_KEY` (1): 10 or 26 hex digits, or 5 or 13 ASCII letters.
    Key,
    /// `NM_WEP_KEY_TYPE_PASSPHRASE` (2): free text up to 64 characters.
    Passphrase,
}

impl WepKeyType {
    pub fn from_nm(raw: u32) -> Self {
        match raw {
            1 => Self::Key,
            2 => Self::Passphrase,
            _ => Self::Unknown,
        }
    }
}

/// How an entry's contents are judged before the Connect button lights up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// `_validateWpaPsk` (`:158-172`): exactly 64 hex digits, or 8..=63 of anything.
    WpaPsk,
    /// `_validateStaticWep` (`:174-198`).
    StaticWep(WepKeyType),
    /// Upstream's default when a secret carries no `validate`: any non-empty text (`:66-68`).
    NonEmpty,
}

impl Validation {
    pub fn accepts(self, value: &str) -> bool {
        match self {
            Self::WpaPsk => {
                if value.chars().count() == 64 {
                    value.chars().all(|c| c.is_ascii_hexdigit())
                } else {
                    (8..=63).contains(&value.chars().count())
                }
            }
            Self::StaticWep(kind) => match kind {
                WepKeyType::Passphrase => value.chars().count() <= 64,
                // Unknown falls into the key branch, as upstream's `if/else if` chain does.
                WepKeyType::Key | WepKeyType::Unknown => match value.chars().count() {
                    10 | 26 => value.chars().all(|c| c.is_ascii_hexdigit()),
                    5 | 13 => value.chars().all(|c| c.is_ascii_alphabetic()),
                    _ => false,
                },
            },
            Self::NonEmpty => !value.is_empty(),
        }
    }
}

/// One entry in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretField {
    /// The entry's hint text — `_('Password')`, `_('Username')`, …
    pub label: String,
    /// The NM setting key this answers, or `None` for a display-only row.
    ///
    /// Upstream's `key: null` (`:299`, `:315`, `:377`): a row that shows context — the identity a
    /// password belongs to, the network's own name — and is drawn non-reactive. It is never sent
    /// back, and it does not gate the Connect button (`:84`).
    pub key: Option<String>,
    /// Non-secret prefill; empty for every password entry (see the module docs).
    pub value: String,
    /// Whether the entry hides what is typed.
    pub password: bool,
    pub validation: Validation,
}

impl SecretField {
    fn entry(label: &str, key: &str, password: bool, validation: Validation) -> Self {
        Self {
            label: label.to_owned(),
            key: Some(key.to_owned()),
            value: String::new(),
            password,
            validation,
        }
    }

    /// A display-only row: prefilled, never answered, never blocking.
    fn display(label: &str, value: &str) -> Self {
        Self {
            label: label.to_owned(),
            key: None,
            value: value.to_owned(),
            password: false,
            validation: Validation::NonEmpty,
        }
    }

    /// Whether this field's current text lets the dialog be submitted. A display-only row is
    /// always satisfied, whatever it holds.
    pub fn accepts(&self, value: &str) -> bool {
        self.key.is_none() || self.validation.accepts(value)
    }
}

/// Everything the dialog needs to put itself up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretContent {
    pub title: String,
    /// The body text under the title; `None` for the types upstream leaves bare (`:369`, `:379`).
    pub message: Option<String>,
    pub fields: Vec<SecretField>,
    /// `WPS_PBC_ACTIVE` was set, so the dialog adds "…you can connect by pushing the WPS button"
    /// (`:94-103`).
    pub wps_available: bool,
}

/// The connection NM is asking about, flattened out of its `a{sa{sv}}`.
///
/// Only the non-secret fields the dialog's shape depends on; the secrets themselves are read from
/// the keyring by the agent and never enter this type.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionInfo {
    /// `connection.type` — `802-11-wireless`, `802-3-ethernet`, `pppoe`, `gsm`, `cdma`,
    /// `bluetooth`, `vpn`.
    pub kind: String,
    /// `connection.id` — the connection's display name.
    pub id: String,
    /// `connection.uuid` — the keyring's identity for it.
    pub uuid: String,
    /// The SSID, decoded from `802-11-wireless.ssid` (`ay`).
    pub ssid: Option<String>,
    /// `802-11-wireless-security.key-mgmt`.
    pub key_mgmt: Option<String>,
    /// `802-11-wireless-security.auth-alg`.
    pub auth_alg: Option<String>,
    /// `802-11-wireless-security.wep-tx-keyidx`.
    pub wep_tx_keyidx: u32,
    /// `802-11-wireless-security.wep-key-type`.
    pub wep_key_type: WepKeyType,
    /// `802-1x.eap`'s first method.
    pub eap: Option<String>,
    /// Non-secret identity text to prefill with, by setting key (`identity`, `username`,
    /// `service`).
    pub identities: HashMap<String, String>,
}

impl ConnectionInfo {
    fn identity(&self, key: &str) -> String {
        self.identities.get(key).cloned().unwrap_or_default()
    }
}

/// The dialog content for one `GetSecrets`, or `None` when there is nothing we can ask
/// (an unknown connection type, or VPN — see the module docs).
pub fn content(
    info: &ConnectionInfo,
    setting_name: &str,
    hints: &[String],
    wps_available: bool,
) -> Option<SecretContent> {
    let mut fields = Vec::new();
    let (title, message) = match info.kind.as_str() {
        "802-11-wireless" => {
            wireless_fields(info, setting_name, hints, &mut fields);
            let ssid = info.ssid.clone().unwrap_or_else(|| info.id.clone());
            (
                "Authentication required".to_owned(),
                Some(format!(
                    "Passwords or encryption keys are required to access the wireless network \u{201c}{ssid}\u{201d}"
                )),
            )
        }
        "802-3-ethernet" => {
            fields.push(SecretField::display("Network name", &info.id));
            eap_fields(info, setting_name, hints, &mut fields);
            ("Wired 802.1X authentication".to_owned(), None)
        }
        "pppoe" => {
            fields.push(SecretField::entry(
                "Username",
                "username",
                false,
                Validation::NonEmpty,
            ));
            fields[0].value = info.identity("username");
            fields.push(SecretField::entry(
                "Service",
                "service",
                false,
                Validation::NonEmpty,
            ));
            fields[1].value = info.identity("service");
            fields.push(SecretField::entry(
                "Password",
                "password",
                true,
                Validation::NonEmpty,
            ));
            ("DSL authentication".to_owned(), None)
        }
        "gsm" if hints.iter().any(|h| h == "pin") => {
            fields.push(SecretField::entry("PIN", "pin", true, Validation::NonEmpty));
            (
                "PIN code required".to_owned(),
                Some("PIN code is needed for the mobile broadband device".to_owned()),
            )
        }
        // `gsm` without a `pin` hint falls through to the mobile branch, as upstream's switch does.
        "gsm" | "cdma" | "bluetooth" => {
            fields.push(SecretField::entry(
                "Password",
                "password",
                true,
                Validation::NonEmpty,
            ));
            (
                "Authentication required".to_owned(),
                Some(format!(
                    "A password is required to connect to \u{201c}{}\u{201d}",
                    info.id
                )),
            )
        }
        _ => return None,
    };

    // Upstream logs and shows an empty dialog when a key management or EAP method falls off the
    // end of its switch; an empty dialog can only be cancelled, so we treat it as "cannot ask"
    // and let the agent tell NM so.
    if fields.iter().all(|f| f.key.is_none()) {
        return None;
    }

    Some(SecretContent {
        title,
        message,
        fields,
        wps_available,
    })
}

/// `_getWirelessSecrets` (`:200-247`).
fn wireless_fields(
    info: &ConnectionInfo,
    setting_name: &str,
    hints: &[String],
    fields: &mut Vec<SecretField>,
) {
    if setting_name == "802-1x" {
        eap_fields(info, setting_name, hints, fields);
        return;
    }

    match info.key_mgmt.as_deref().unwrap_or_default() {
        "wpa-none" | "wpa-psk" | "sae" => fields.push(SecretField::entry(
            "Password",
            "psk",
            true,
            Validation::WpaPsk,
        )),
        // Static WEP. The key index is part of the *key name*, so it must be read, not assumed.
        "none" => fields.push(SecretField::entry(
            "Key",
            &format!("wep-key{}", info.wep_tx_keyidx),
            true,
            Validation::StaticWep(info.wep_key_type),
        )),
        "ieee8021x" => {
            if info.auth_alg.as_deref() == Some("leap") {
                fields.push(SecretField::entry(
                    "Password",
                    "leap-password",
                    true,
                    Validation::NonEmpty,
                ));
            } else {
                eap_fields(info, setting_name, hints, fields);
            }
        }
        "wpa-eap" => eap_fields(info, setting_name, hints, fields),
        other => warn!("network agent: unhandled wireless key management {other:?}"),
    }
}

/// `_get8021xSecrets` (`:249-320`).
///
/// The hint path comes first and is exact: when NM says which keys it wants, ask for those and
/// nothing else. Only in its absence do we guess from the EAP method.
fn eap_fields(
    info: &ConnectionInfo,
    setting_name: &str,
    hints: &[String],
    fields: &mut Vec<SecretField>,
) {
    if setting_name == "802-1x" && !hints.is_empty() {
        if hints.iter().any(|h| h == "identity") {
            let mut field = SecretField::entry("Username", "identity", false, Validation::NonEmpty);
            field.value = info.identity("identity");
            fields.push(field);
        }
        if hints.iter().any(|h| h == "password") {
            fields.push(SecretField::entry(
                "Password",
                "password",
                true,
                Validation::NonEmpty,
            ));
        }
        if hints.iter().any(|h| h == "private-key-password") {
            fields.push(SecretField::entry(
                "Private key password",
                "private-key-password",
                true,
                Validation::NonEmpty,
            ));
        }
        return;
    }

    match info.eap.as_deref().unwrap_or_default() {
        // TTLS and PEAP are much more complicated than this, but the complication is invisible
        // here: only phase-2 authentication is being asked for. Upstream says the same
        // (`:284-286`).
        "md5" | "leap" | "ttls" | "peap" | "fast" => {
            fields.push(SecretField::display("Username", &info.identity("identity")));
            fields.push(SecretField::entry(
                "Password",
                "password",
                true,
                Validation::NonEmpty,
            ));
        }
        "tls" => {
            fields.push(SecretField::display("Identity", &info.identity("identity")));
            fields.push(SecretField::entry(
                "Private key password",
                "private-key-password",
                true,
                Validation::NonEmpty,
            ));
        }
        other => warn!("network agent: unhandled EAP method {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wifi(key_mgmt: &str) -> ConnectionInfo {
        ConnectionInfo {
            kind: "802-11-wireless".to_owned(),
            id: "Café".to_owned(),
            uuid: "u".to_owned(),
            ssid: Some("Café".to_owned()),
            key_mgmt: Some(key_mgmt.to_owned()),
            ..Default::default()
        }
    }

    #[test]
    fn wpa_psk_asks_for_one_password() {
        let c = content(&wifi("wpa-psk"), "802-11-wireless-security", &[], false).unwrap();
        assert_eq!(c.title, "Authentication required");
        assert!(c.message.unwrap().contains("\u{201c}Café\u{201d}"));
        assert_eq!(c.fields.len(), 1);
        assert_eq!(c.fields[0].key.as_deref(), Some("psk"));
        assert!(c.fields[0].password);
        assert_eq!(c.fields[0].validation, Validation::WpaPsk);
    }

    #[test]
    fn a_wpa_password_is_eight_to_sixtythree_or_sixtyfour_hex() {
        let v = Validation::WpaPsk;
        assert!(!v.accepts("short"));
        assert!(v.accepts("12345678"));
        assert!(v.accepts(&"x".repeat(63)));
        assert!(!v.accepts(&"x".repeat(65)));
        // 64 characters is the raw-key spelling, and then it must be hex.
        assert!(v.accepts(&"a".repeat(64)));
        assert!(!v.accepts(&"z".repeat(64)));
    }

    #[test]
    fn a_static_wep_key_name_carries_its_index() {
        let mut info = wifi("none");
        info.wep_tx_keyidx = 2;
        info.wep_key_type = WepKeyType::Key;
        let c = content(&info, "802-11-wireless-security", &[], false).unwrap();
        assert_eq!(c.fields[0].key.as_deref(), Some("wep-key2"));
        assert_eq!(
            c.fields[0].validation,
            Validation::StaticWep(WepKeyType::Key)
        );
    }

    #[test]
    fn a_wep_key_is_hex_or_ascii_at_four_exact_lengths() {
        let v = Validation::StaticWep(WepKeyType::Key);
        assert!(v.accepts("abcde")); // 5 ASCII
        assert!(v.accepts("0123456789")); // 10 hex
        assert!(!v.accepts("0123456789a")); // 11 of anything
        assert!(!v.accepts("zzzzzzzzzz")); // 10, but not hex
        assert!(Validation::StaticWep(WepKeyType::Passphrase).accepts(""));
        assert!(!Validation::StaticWep(WepKeyType::Passphrase).accepts(&"x".repeat(65)));
    }

    #[test]
    fn hints_pick_the_exact_eight_o_two_dot_one_x_fields() {
        let mut info = wifi("wpa-eap");
        info.identities
            .insert("identity".to_owned(), "kov".to_owned());
        let hints = ["identity".to_owned(), "password".to_owned()];
        let c = content(&info, "802-1x", &hints, false).unwrap();
        let keys: Vec<_> = c.fields.iter().filter_map(|f| f.key.as_deref()).collect();
        assert_eq!(keys, ["identity", "password"]);
        assert_eq!(c.fields[0].value, "kov", "non-secret identity prefills");
    }

    #[test]
    fn without_hints_the_eap_method_decides() {
        let mut info = wifi("wpa-eap");
        info.eap = Some("peap".to_owned());
        info.identities
            .insert("identity".to_owned(), "kov".to_owned());
        let c = content(&info, "802-11-wireless-security", &[], false).unwrap();
        // The username is shown, not asked: upstream gives it `key: null`.
        assert_eq!(c.fields[0].key, None);
        assert_eq!(c.fields[0].value, "kov");
        assert_eq!(c.fields[1].key.as_deref(), Some("password"));
    }

    #[test]
    fn a_password_field_is_never_prefilled() {
        let mut info = wifi("wpa-psk");
        info.identities
            .insert("psk".to_owned(), "leaked".to_owned());
        let c = content(&info, "802-11-wireless-security", &[], false).unwrap();
        assert!(c.fields.iter().all(|f| !f.password || f.value.is_empty()));
    }

    #[test]
    fn wired_eight_o_two_dot_one_x_shows_the_connection_name() {
        let info = ConnectionInfo {
            kind: "802-3-ethernet".to_owned(),
            id: "Office wired".to_owned(),
            eap: Some("ttls".to_owned()),
            ..Default::default()
        };
        let c = content(&info, "802-1x", &[], false).unwrap();
        assert_eq!(c.title, "Wired 802.1X authentication");
        assert_eq!(c.message, None);
        assert_eq!(c.fields[0].label, "Network name");
        assert_eq!(c.fields[0].value, "Office wired");
        assert_eq!(c.fields[0].key, None);
    }

    #[test]
    fn a_gsm_pin_hint_changes_the_whole_dialog() {
        let info = ConnectionInfo {
            kind: "gsm".to_owned(),
            id: "Carrier".to_owned(),
            ..Default::default()
        };
        let pin = content(&info, "gsm", &["pin".to_owned()], false).unwrap();
        assert_eq!(pin.title, "PIN code required");
        assert_eq!(pin.fields[0].key.as_deref(), Some("pin"));

        let plain = content(&info, "gsm", &[], false).unwrap();
        assert_eq!(plain.title, "Authentication required");
        assert_eq!(plain.fields[0].key.as_deref(), Some("password"));
    }

    #[test]
    fn vpn_and_the_unknown_are_refused_rather_than_shown_empty() {
        let vpn = ConnectionInfo {
            kind: "vpn".to_owned(),
            ..Default::default()
        };
        assert_eq!(content(&vpn, "vpn", &[], false), None);

        // A wireless connection whose key management we cannot ask for would otherwise produce a
        // dialog with no answerable entry — a box that can only be cancelled.
        assert_eq!(
            content(
                &wifi("wpa-eap-suite-b-192"),
                "802-11-wireless-security",
                &[],
                false
            ),
            None
        );
    }

    #[test]
    fn a_display_row_never_blocks_the_button() {
        let row = SecretField::display("Username", "");
        assert!(row.accepts(""), "an empty display row is still satisfied");
        let entry = SecretField::entry("Password", "psk", true, Validation::WpaPsk);
        assert!(!entry.accepts(""));
    }
}
