// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::xdg_activation::XdgActivationTokenData;
use synoik_config::PresetSize;

use super::ResolvedWindowRules;

#[derive(Debug)]
pub struct Unmapped {
    pub window: Window,
    pub state: InitialConfigureState,
    /// Activation token, if one was used on this unmapped window.
    pub activation_token_data: Option<XdgActivationTokenData>,
    /// The token *string*, kept alongside the data so a mapping window can be
    /// matched against an open startup sequence by id — mutter's
    /// `xdg_activation_token_lookup` path (`meta-wayland-activation.c:339-347`).
    pub activation_token: Option<String>,
    /// Whether the client has performed the initial commit on this toplevel's surface.
    ///
    /// Not the same as having been configured: the initial configure is sent from an idle, so
    /// between the commit and that idle the window is still `NotConfigured`. Requests that the
    /// spec pins to "before the first commit" — `xdg_toplevel_session_v1`'s `already_mapped`
    /// error — need the commit itself, not the configure.
    pub had_initial_commit: bool,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum InitialConfigureState {
    /// The window has not been initially configured yet.
    NotConfigured {
        /// Whether the window requested to be fullscreened, and the requested output, if any.
        wants_fullscreen: Option<Option<Output>>,

        /// Whether the window requested to be maximized.
        wants_maximized: bool,
    },
    /// The window has been configured.
    Configured {
        /// Up-to-date rules.
        ///
        /// We start tracking window rules when sending the initial configure, since they don't
        /// affect anything before that.
        rules: ResolvedWindowRules,

        /// Resolved scrolling default width for this window.
        ///
        /// `None` means that the window will pick its own width.
        width: Option<PresetSize>,

        /// Resolved scrolling default height for this window.
        ///
        /// `None` means that the window will pick its own height.
        height: Option<PresetSize>,

        /// Resolved floating default width for this window.
        ///
        /// `None` means that the window will pick its own width.
        floating_width: Option<PresetSize>,

        /// Resolved floating default height for this window.
        ///
        /// `None` means that the window will pick its own height.
        floating_height: Option<PresetSize>,

        /// Whether the window should open full-width.
        is_full_width: bool,

        /// Output to open this window on.
        ///
        /// This can be `None` in cases like:
        ///
        /// - There are no outputs connected.
        /// - This is a dialog with a parent, and there was no explicit output set, so this dialog
        ///   should fetch the parent's current output again upon mapping.
        output: Option<Output>,

        /// Workspace to open this window on.
        workspace_name: Option<String>,

        /// Whether the window should be maximized.
        ///
        /// This corresponds to the window having the Maximized toplevel state. However, if the
        /// window is also pending fullscreen, then it has the Fullscreen toplevel state, so we
        /// need to store pending maximized elsewhere, hence this field.
        is_pending_maximized: bool,
    },
}

impl Unmapped {
    /// Wraps a newly created window that hasn't been initially configured yet.
    pub fn new(window: Window) -> Self {
        Self {
            window,
            state: InitialConfigureState::NotConfigured {
                wants_fullscreen: None,
                wants_maximized: false,
            },
            activation_token_data: None,
            activation_token: None,
            had_initial_commit: false,
        }
    }

    pub fn needs_initial_configure(&self) -> bool {
        matches!(self.state, InitialConfigureState::NotConfigured { .. })
    }

    pub fn toplevel(&self) -> &ToplevelSurface {
        self.window.toplevel().expect("no X11 support")
    }
}
