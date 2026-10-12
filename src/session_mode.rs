// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Session modes — gnome-shell's `js/ui/sessionMode.js`: one record per mode saying what the shell
//! may show in it. A surface asks the current mode rather than keeping its own idea of "locked"
//! or "greeter" (`docs/fork/greeter.md` §4).
//!
//! Three modes, as gnome-shell ships them (`sessionMode.js:22-102`): `user`, `unlock-dialog`
//! (pushed over `user` while the screen shield is up) and `gdm` (the greeter, `synoik --greeter`).
//! Every field defaults to `restrictive`'s, the mode a mode inherits what it does not say from.

use crate::ui::panel::{
    ROLE_A11Y, ROLE_ACTIVITIES, ROLE_APP_INDICATORS, ROLE_DATE_MENU, ROLE_KEYBOARD,
    ROLE_QUICK_SETTINGS, ROLE_SCREEN_RECORDING,
};

/// The panel's look in a mode (`panelStyle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelStyle {
    /// The normal bar.
    Normal,
    /// `.unlock-screen` / `.login-screen` (`_panel.scss:100-130`): no background, white text.
    Transparent,
}

/// One mode's record. Only the flags something reads are here; the rest of a mode (overview,
/// windows, run dialog, screencast, notifications) is enforced by the shield's own gates for now,
/// and joins as a surface starts asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMode {
    pub name: &'static str,
    pub allow_settings: bool,
    pub has_notifications: bool,
    pub is_locked: bool,
    pub is_greeter: bool,
    /// Every panel role the mode shows, across its three boxes.
    pub panel_roles: &'static [&'static str],
    pub panel_style: PanelStyle,
}

impl SessionMode {
    pub fn shows_panel_role(&self, role: &str) -> bool {
        self.panel_roles.contains(&role)
    }
}

/// `restrictive` (`sessionMode.js:22-48`), the defaults the others override.
const RESTRICTIVE: SessionMode = SessionMode {
    name: "restrictive",
    allow_settings: false,
    has_notifications: false,
    is_locked: false,
    is_greeter: false,
    panel_roles: &[],
    panel_style: PanelStyle::Normal,
};

/// `user` (`sessionMode.js:79-101`). The app indicators are ours (no GNOME role) and live where
/// a user's session does.
pub const USER: SessionMode = SessionMode {
    name: "user",
    allow_settings: true,
    has_notifications: true,
    panel_roles: &[
        ROLE_ACTIVITIES,
        ROLE_DATE_MENU,
        ROLE_APP_INDICATORS,
        ROLE_SCREEN_RECORDING,
        ROLE_A11Y,
        ROLE_KEYBOARD,
        ROLE_QUICK_SETTINGS,
    ],
    ..RESTRICTIVE
};

/// `unlock-dialog` (`sessionMode.js:65-77`): the lock screen. No clock in the panel — the shield
/// shows its own.
pub const UNLOCK_DIALOG: SessionMode = SessionMode {
    name: "unlock-dialog",
    is_locked: true,
    panel_roles: &[ROLE_A11Y, ROLE_KEYBOARD, ROLE_QUICK_SETTINGS],
    panel_style: PanelStyle::Transparent,
    ..RESTRICTIVE
};

/// `gdm` (`sessionMode.js:50-63`): the greeter. a11y is the login dialog's own button rather than
/// a panel item.
pub const GDM: SessionMode = SessionMode {
    name: "gdm",
    has_notifications: true,
    is_greeter: true,
    panel_roles: &[ROLE_DATE_MENU, ROLE_KEYBOARD, ROLE_QUICK_SETTINGS],
    panel_style: PanelStyle::Transparent,
    ..RESTRICTIVE
};

/// The mode in force: the greeter is always `gdm`; a user's session is `unlock-dialog` while its
/// screen is covered, `user` otherwise.
pub fn current(greeter: bool, screen_covered: bool) -> &'static SessionMode {
    if greeter {
        &GDM
    } else if screen_covered {
        &UNLOCK_DIALOG
    } else {
        &USER
    }
}
