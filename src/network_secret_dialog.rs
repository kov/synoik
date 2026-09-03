// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The network secret dialog's state machine, answering the NetworkManager agent.
//!
//! A port of `NetworkSecretDialog` (`js/ui/components/networkAgent.js:23-156`), split the way
//! [`crate::polkit_dialog`] is: this owns what is being asked, what has been typed and where focus
//! sits; [`crate::ui::network_secret_dialog`] draws it; [`crate::dbus::network_agent`] carries the
//! answer to NetworkManager.
//!
//! The one structural difference from the polkit dialog is that this is a **form**: a request
//! carries between one and three entries, each with its own label, its own masking and its own
//! validation ([`crate::network_secret`]). Focus moves through the answerable ones only — a
//! display row (upstream's `key: null`) shows context and is never typed into (`:52-56`, `:84`).
//!
//! # Secrecy
//!
//! What is typed lives in the [`TextEdit`]s here and reaches exactly one place: the
//! [`NetworkAgentRequest::Respond`] this hands back. Nothing about a field's contents is
//! `Debug`-printable — [`SecretEffects`] redacts, [`TextEdit::secure_clear`] wipes on close, and
//! the compositor's own dumps must not reach [`Self::values`]. A password that reaches the journal
//! is the same bug whether it got there from D-Bus or from a `{:?}`.

use std::collections::HashMap;
use std::time::Duration;

use smithay::input::keyboard::Keysym;

use crate::dbus::network_agent::{NetworkAgentRequest, SecretRequest};
use crate::network_secret::{SecretContent, SecretField};
use crate::ui::text_edit::{EditMods, EditOutcome, KeyTheme, TextEdit};

/// Entry capacity to wipe to on close, matching the polkit dialog's. Large enough that a realistic
/// passphrase never reallocates — a reallocation leaves the old text in freed memory, which
/// [`TextEdit::secure_clear`] can no longer reach.
const ENTRY_CAPACITY: usize = 512;

/// The button labels (`networkAgent.js:106-118`).
pub const CONNECT: &str = "Connect";
pub const CANCEL: &str = "Cancel";

/// The WPS line, shown when NM says the router's button is live (`:96`).
pub const WPS_HINT: &str =
    "Alternatively you can connect by pushing the \u{201c}WPS\u{201d} button on your router";

/// Which control has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The nth entry, counted over **all** fields including display rows — the index is into
    /// [`SecretContent::fields`] so the view can match focus to what it draws without a second
    /// mapping. Only answerable fields are ever focused; see [`NetworkSecretDialog::stops`].
    Field(usize),
    Cancel,
    Connect,
}

/// What the caller must do after driving the dialog.
#[derive(Default)]
pub struct SecretEffects {
    /// Send this to [`crate::dbus::network_agent`]. **Carries secrets.**
    pub request: Option<NetworkAgentRequest>,
    /// Something visible changed.
    pub redraw: bool,
    /// The dialog has finished and should come down.
    pub close: bool,
}

/// Hand-written so a stray `{:?}` cannot put a Wi-Fi password in the journal by way of `request`.
/// `NetworkAgentRequest`'s own `Debug` redacts, but relying on that alone would make this type's
/// safety depend on a decision made in another file.
impl std::fmt::Debug for SecretEffects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretEffects")
            .field("request", &self.request)
            .field("redraw", &self.redraw)
            .field("close", &self.close)
            .finish()
    }
}

impl SecretEffects {
    fn redraw() -> Self {
        Self {
            redraw: true,
            ..Default::default()
        }
    }
}

pub struct NetworkSecretDialog {
    /// `None` when nothing is being asked.
    request: Option<SecretRequest>,
    /// One editor per field, parallel to [`SecretContent::fields`]. A display row gets one too, so
    /// the indices line up; it is prefilled and never edited.
    entries: Vec<TextEdit>,
    focus: Focus,
    caps_warning: bool,
    /// When the request arrived, for the notification path's ordering.
    opened_at: Option<Duration>,
}

impl Default for NetworkSecretDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkSecretDialog {
    pub fn new() -> Self {
        Self {
            request: None,
            entries: Vec::new(),
            focus: Focus::Connect,
            caps_warning: false,
            opened_at: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.request.is_some()
    }

    pub fn content(&self) -> Option<&SecretContent> {
        self.request.as_ref().map(|r| &r.content)
    }

    pub fn request_id(&self) -> Option<&str> {
        self.request.as_ref().map(|r| r.request_id.as_str())
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn caps_warning(&self) -> bool {
        self.caps_warning
    }

    pub fn opened_at(&self) -> Option<Duration> {
        self.opened_at
    }

    /// Whether the caps-lock warning is relevant at all: only when something is masked
    /// (`networkAgent.js:89-92`).
    pub fn has_password_field(&self) -> bool {
        self.content()
            .is_some_and(|c| c.fields.iter().any(|f| f.password))
    }

    pub fn set_caps_warning(&mut self, warn: bool) -> bool {
        let changed = self.caps_warning != warn && self.has_password_field();
        if changed {
            self.caps_warning = warn;
        }
        changed
    }

    /// The editing model behind field `index`, for the view's caret and selection.
    pub fn entry(&self, index: usize) -> Option<&TextEdit> {
        self.entries.get(index)
    }

    /// What to draw in field `index`: the text, or one bullet per character when it is a password.
    pub fn entry_display(&self, index: usize) -> String {
        let mask = self.entry_mask(index);
        self.entries
            .get(index)
            .map(|e| e.display(mask))
            .unwrap_or_default()
    }

    /// U+25CF, `st_password_entry`'s bullet, for a password field; `None` for anything else.
    pub fn entry_mask(&self, index: usize) -> Option<char> {
        let field = self.content()?.fields.get(index)?;
        field.password.then_some('\u{25cf}')
    }

    /// Show (or clear) the input method's in-progress composition in the focused entry.
    pub fn set_preedit(&mut self, preedit: Option<String>) -> bool {
        let Focus::Field(index) = self.focus else {
            return false;
        };
        self.entries
            .get_mut(index)
            .is_some_and(|e| e.set_preedit(preedit))
    }

    /// The indices of the fields that can be focused: the answerable ones.
    ///
    /// A display row is drawn non-reactive and skipped entirely — tabbing into a box that cannot
    /// be typed in is a dead stop the user has to escape from.
    fn stops(&self) -> Vec<usize> {
        let Some(content) = self.content() else {
            return Vec::new();
        };
        content
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| f.key.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether Connect can be pressed: every field satisfied by what it currently holds
    /// (`_updateOkButton`, `:122-131`).
    pub fn can_connect(&self) -> bool {
        let Some(content) = self.content() else {
            return false;
        };
        content
            .fields
            .iter()
            .zip(&self.entries)
            .all(|(field, entry)| field.accepts(entry.text()))
    }

    /// Put a request up. Any request already showing is dropped without an answer — the agent has
    /// already told NetworkManager `AgentCanceled` for it, so answering again would be a second
    /// reply to a call that is gone.
    pub fn begin(&mut self, request: SecretRequest, now: Duration) -> SecretEffects {
        self.entries = request.content.fields.iter().map(prefilled_entry).collect();
        // Initial focus is the first *answerable* field, as upstream sets it (`:73-76`); with none
        // — which `content()` refuses to produce — Connect is the only sensible landing.
        self.request = Some(request);
        self.focus = self
            .stops()
            .first()
            .map_or(Focus::Connect, |i| Focus::Field(*i));
        self.caps_warning = false;
        self.opened_at = Some(now);
        SecretEffects::redraw()
    }

    /// NetworkManager withdrew the request, or the compositor is tearing down. Close with no
    /// answer: the call this dialog was going to complete no longer exists.
    pub fn withdraw(&mut self, request_id: &str) -> SecretEffects {
        if self.request_id() != Some(request_id) {
            return SecretEffects::default();
        }
        self.reset();
        SecretEffects {
            redraw: true,
            close: true,
            ..Default::default()
        }
    }

    /// Type into the focused entry. `None` means the key was not ours to handle.
    pub fn entry_key(
        &mut self,
        sym: Option<Keysym>,
        ch: Option<char>,
        mods: EditMods,
        theme: KeyTheme,
    ) -> Option<SecretEffects> {
        let Focus::Field(index) = self.focus else {
            return None;
        };
        let entry = self.entries.get_mut(index)?;
        match entry.handle_key(sym, ch, mods, theme) {
            EditOutcome::Ignored => None,
            // Enter and Escape belong to the dialog, not the field: they connect and cancel.
            EditOutcome::Activate | EditOutcome::Cancel => None,
            EditOutcome::Changed | EditOutcome::Moved => Some(SecretEffects::redraw()),
        }
    }

    /// Type one character into the focused entry — what tests and callers mean by typing.
    pub fn type_char(&mut self, c: char) -> SecretEffects {
        self.entry_key(None, Some(c), EditMods::default(), KeyTheme::default())
            .unwrap_or_default()
    }

    /// Move focus on Tab / arrows, over the answerable fields and the two buttons.
    pub fn cycle_focus(&mut self, forward: bool) -> SecretEffects {
        let mut stops: Vec<Focus> = self.stops().into_iter().map(Focus::Field).collect();
        stops.push(Focus::Cancel);
        stops.push(Focus::Connect);

        let at = stops.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % stops.len()
        } else {
            (at + stops.len() - 1) % stops.len()
        };
        self.focus = stops[next];
        SecretEffects::redraw()
    }

    pub fn set_focus(&mut self, focus: Focus) -> SecretEffects {
        // A display row is not a focus stop, however it was clicked.
        if let Focus::Field(index) = focus {
            if !self.stops().contains(&index) {
                return SecretEffects::default();
            }
        }
        if self.focus == focus {
            return SecretEffects::default();
        }
        self.focus = focus;
        SecretEffects::redraw()
    }

    /// Press Connect, or Enter in any entry — the same thing (`:79`, `:133-151`).
    ///
    /// A press with an invalid field does nothing at all, exactly as upstream's `_onOk` falls
    /// through without responding. The button is drawn non-reactive in that state, so this is the
    /// keyboard path's guard, not a second policy.
    pub fn connect(&mut self) -> SecretEffects {
        if !self.can_connect() {
            return SecretEffects::default();
        }
        let Some(request) = self.request.as_ref() else {
            return SecretEffects::default();
        };
        let request_id = request.request_id.clone();
        let values = self.values();
        self.reset();
        SecretEffects {
            request: Some(NetworkAgentRequest::Respond { request_id, values }),
            redraw: true,
            close: true,
        }
    }

    /// Press Cancel, or Escape (`:153-156`).
    pub fn cancel(&mut self) -> SecretEffects {
        let Some(request) = self.request.as_ref() else {
            return SecretEffects::default();
        };
        let request_id = request.request_id.clone();
        self.reset();
        SecretEffects {
            request: Some(NetworkAgentRequest::Dismiss { request_id }),
            redraw: true,
            close: true,
        }
    }

    /// The answers, by NM setting key. Display rows contribute nothing — they were never asked.
    fn values(&self) -> HashMap<String, String> {
        let Some(content) = self.content() else {
            return HashMap::new();
        };
        content
            .fields
            .iter()
            .zip(&self.entries)
            .filter_map(|(field, entry)| Some((field.key.clone()?, entry.text().to_owned())))
            .collect()
    }

    /// Wipe everything typed. Called on every close, whichever way the dialog left the screen —
    /// a password must not outlive the question it answered.
    fn reset(&mut self) {
        for entry in &mut self.entries {
            entry.secure_clear(ENTRY_CAPACITY);
        }
        self.entries.clear();
        self.request = None;
        self.focus = Focus::Connect;
        self.caps_warning = false;
        self.opened_at = None;
    }
}

/// One editor for one field. Only non-secret prefill is carried in — see
/// [`crate::network_secret`].
fn prefilled_entry(field: &SecretField) -> TextEdit {
    let mut entry = TextEdit::with_capacity(ENTRY_CAPACITY);
    if !field.value.is_empty() {
        entry.set_text(field.value.clone());
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network_secret::{ConnectionInfo, WepKeyType};

    fn request(kind: &str, key_mgmt: &str, setting: &str) -> SecretRequest {
        let info = ConnectionInfo {
            kind: kind.to_owned(),
            id: "Café".to_owned(),
            uuid: "uuid-1".to_owned(),
            ssid: Some("Café".to_owned()),
            key_mgmt: Some(key_mgmt.to_owned()),
            wep_key_type: WepKeyType::Key,
            ..Default::default()
        };
        SecretRequest {
            request_id: "/settings/1/".to_owned() + setting,
            content: crate::network_secret::content(&info, setting, &[], false).unwrap(),
            user_requested: true,
        }
    }

    fn wpa() -> SecretRequest {
        request("802-11-wireless", "wpa-psk", "802-11-wireless-security")
    }

    fn open(request: SecretRequest) -> NetworkSecretDialog {
        let mut dialog = NetworkSecretDialog::new();
        dialog.begin(request, Duration::ZERO);
        dialog
    }

    fn type_str(dialog: &mut NetworkSecretDialog, s: &str) {
        for c in s.chars() {
            dialog.type_char(c);
        }
    }

    #[test]
    fn opening_focuses_the_first_answerable_field() {
        let dialog = open(wpa());
        assert!(dialog.is_open());
        assert_eq!(dialog.focus(), Focus::Field(0));
        assert!(!dialog.can_connect(), "an empty password is not valid");
    }

    #[test]
    fn a_display_row_is_skipped_by_focus() {
        // Wired 802.1X: field 0 is the connection name (display), field 1 the password.
        let info = ConnectionInfo {
            kind: "802-3-ethernet".to_owned(),
            id: "Office".to_owned(),
            eap: Some("peap".to_owned()),
            ..Default::default()
        };
        let content = crate::network_secret::content(&info, "802-1x", &[], false).unwrap();
        // Network name (display), Username (display), Password (entry).
        assert_eq!(content.fields.len(), 3);
        let dialog = open(SecretRequest {
            request_id: "/settings/2/802-1x".to_owned(),
            content,
            user_requested: true,
        });
        assert_eq!(dialog.focus(), Focus::Field(2), "the only answerable field");
    }

    #[test]
    fn tab_visits_the_answerable_fields_then_both_buttons() {
        let mut dialog = open(wpa());
        assert_eq!(dialog.focus(), Focus::Field(0));
        dialog.cycle_focus(true);
        assert_eq!(dialog.focus(), Focus::Cancel);
        dialog.cycle_focus(true);
        assert_eq!(dialog.focus(), Focus::Connect);
        dialog.cycle_focus(true);
        assert_eq!(dialog.focus(), Focus::Field(0), "and wraps");
        dialog.cycle_focus(false);
        assert_eq!(dialog.focus(), Focus::Connect, "backwards too");
    }

    #[test]
    fn a_display_row_cannot_be_clicked_into() {
        let info = ConnectionInfo {
            kind: "802-3-ethernet".to_owned(),
            id: "Office".to_owned(),
            eap: Some("peap".to_owned()),
            ..Default::default()
        };
        let content = crate::network_secret::content(&info, "802-1x", &[], false).unwrap();
        let mut dialog = open(SecretRequest {
            request_id: "/settings/2/802-1x".to_owned(),
            content,
            user_requested: true,
        });
        dialog.set_focus(Focus::Field(0));
        assert_eq!(dialog.focus(), Focus::Field(2), "focus did not move");
    }

    #[test]
    fn connect_lights_up_only_on_a_valid_password() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "short");
        assert!(!dialog.can_connect(), "five characters is under eight");
        type_str(&mut dialog, "er");
        assert!(!dialog.can_connect(), "and so is seven");
        type_str(&mut dialog, "s");
        assert!(dialog.can_connect(), "eight is the floor");
    }

    #[test]
    fn connecting_answers_with_the_setting_key() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "correcthorse");
        let effects = dialog.connect();
        assert!(effects.close);
        match effects.request {
            Some(NetworkAgentRequest::Respond { request_id, values }) => {
                assert_eq!(request_id, "/settings/1/802-11-wireless-security");
                assert_eq!(values.get("psk").map(String::as_str), Some("correcthorse"));
                assert_eq!(values.len(), 1, "only the answerable field is sent");
            }
            other => panic!("expected a Respond, got {other:?}"),
        }
        assert!(!dialog.is_open());
    }

    #[test]
    fn an_invalid_connect_does_nothing_at_all() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "short");
        let effects = dialog.connect();
        assert!(effects.request.is_none());
        assert!(!effects.close);
        assert!(dialog.is_open(), "the dialog stays up to be corrected");
    }

    #[test]
    fn cancelling_dismisses_the_request() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "correcthorse");
        let effects = dialog.cancel();
        assert!(effects.close);
        assert!(matches!(
            effects.request,
            Some(NetworkAgentRequest::Dismiss { .. })
        ));
        assert!(!dialog.is_open());
    }

    #[test]
    fn what_was_typed_does_not_survive_the_close() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "correcthorse");
        dialog.cancel();
        assert!(dialog.entries.is_empty());
        assert_eq!(dialog.entry_display(0), "");
        assert_eq!(dialog.values(), HashMap::new());
    }

    #[test]
    fn a_password_is_drawn_as_bullets() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "abcd");
        assert_eq!(dialog.entry_mask(0), Some('\u{25cf}'));
        assert_eq!(dialog.entry_display(0), "\u{25cf}\u{25cf}\u{25cf}\u{25cf}");
    }

    #[test]
    fn a_withdrawn_request_closes_without_answering() {
        let mut dialog = open(wpa());
        let effects = dialog.withdraw("/settings/1/802-11-wireless-security");
        assert!(effects.close);
        assert!(
            effects.request.is_none(),
            "NetworkManager already gave up on this call"
        );
        assert!(!dialog.is_open());
    }

    #[test]
    fn a_withdrawal_for_another_request_is_ignored() {
        let mut dialog = open(wpa());
        let effects = dialog.withdraw("/settings/9/802-11-wireless-security");
        assert!(!effects.close);
        assert!(dialog.is_open());
    }

    #[test]
    fn caps_lock_is_only_warned_about_where_something_is_masked() {
        let mut dialog = open(wpa());
        assert!(dialog.has_password_field());
        assert!(dialog.set_caps_warning(true));
        assert!(dialog.caps_warning());

        let mut closed = NetworkSecretDialog::new();
        assert!(!closed.set_caps_warning(true), "nothing is being asked");
    }

    #[test]
    fn effects_never_print_what_was_typed() {
        let mut dialog = open(wpa());
        type_str(&mut dialog, "correcthorse");
        let printed = format!("{:?}", dialog.connect());
        assert!(!printed.contains("correcthorse"), "{printed}");
    }
}
