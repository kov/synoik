// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The logout / power-off / restart confirmation dialog.
//!
//! gnome-session raises this by calling `org.gnome.SessionManager.EndSessionDialog.Open` on the
//! shell (see `dbus::gnome_session`); this is the compositor-side of gnome-shell's
//! `js/ui/endSessionDialog.js`. The dialog lifecycle and the countdown auto-confirm live in the
//! pure [`crate::end_session::EndSession`] state machine; this widget is only the interactive
//! surface — the open/close animation, which button has focus, and the CPU-rendered texture.
//!
//! It is a [`MessageDialog`]: `Cancel` and the action (`Log Out` / `Power Off` / `Restart` /
//! `Restart & Install`) sharing the button row, a title and a counting-down description above, and
//! the update checkbox between them when there is one.
//!
//! What is drawn is a [`crate::end_session::Presentation`], not the wire dialog type: gnome-shell's
//! fourth presentation, `UPDATE_RESTART`, is one the session manager never asks for. Content
//! (presentation, seconds left, checkbox) is passed in at render time so the state machine stays
//! the single source of truth.

use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::Clock;
use crate::end_session::Presentation;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::system_compositor::Style;
use crate::ui::message_dialog::{
    self, DialogButton, DialogCheck, DialogContent, MessageDialog, MessageDialogRenderElement,
};

/// `.end-session-dialog { width: 24em }` (`_dialogs.scss:51-52`).
const WIDTH_EM: f64 = 24.;
/// `checkBoxText` on the shutdown and restart content (`js/ui/endSessionDialog.js:77`, `:96`).
const CHECK_LABEL: &str = "Install pending software updates";
/// The buttons, in `_updateButtons` order: Cancel first, then the action.
const CANCEL: usize = 0;
const ACTION: usize = 1;

/// What an input event on the dialog asks the compositor to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogOutcome {
    /// Consumed; nothing else to do (swallow the event).
    Handled,
    /// The user activated the action button (or pressed Enter on it): confirm the session action.
    Confirm,
    /// The user pressed Cancel / Esc: abort.
    Cancel,
    /// The user toggled the "Install pending software updates" checkbox.
    ToggleUpdates,
}

impl From<message_dialog::DialogOutcome> for DialogOutcome {
    fn from(outcome: message_dialog::DialogOutcome) -> Self {
        match outcome {
            message_dialog::DialogOutcome::Handled => DialogOutcome::Handled,
            message_dialog::DialogOutcome::Button(CANCEL) => DialogOutcome::Cancel,
            message_dialog::DialogOutcome::Button(ACTION) => DialogOutcome::Confirm,
            message_dialog::DialogOutcome::Button(_) => DialogOutcome::Handled,
            message_dialog::DialogOutcome::ToggleCheck => DialogOutcome::ToggleUpdates,
        }
    }
}

pub type EndSessionDialogRenderElement = MessageDialogRenderElement;

pub struct EndSessionDialog {
    dialog: MessageDialog,
}

impl EndSessionDialog {
    pub fn new(clock: Clock) -> Self {
        Self {
            dialog: MessageDialog::new(clock),
        }
    }

    /// How the system compositor moves switches, if nested in one.
    pub fn set_motion(&mut self, motion: Option<Style>) {
        self.dialog.set_motion(motion);
    }

    /// Raise the dialog for `presentation`, focus on the action button.
    pub fn show(&mut self, presentation: Presentation) {
        self.dialog.show(dialog_content(presentation, None, None));
    }

    /// Update the displayed presentation, countdown and update checkbox (the state machine drives
    /// this each second, and again when gnome-software answers — which can change the presentation
    /// as well as the checkbox).
    pub fn set_content(
        &mut self,
        presentation: Presentation,
        seconds_left: Option<u64>,
        checkbox: Option<bool>,
    ) {
        self.dialog
            .set_content(dialog_content(presentation, seconds_left, checkbox));
    }

    pub fn hide(&mut self) {
        self.dialog.hide();
    }

    /// Confirmed: hide it, or under the system compositor's motion hold it for the greeter or the
    /// shutdown splash to come in over it.
    pub fn confirmed(&mut self) {
        self.dialog.confirmed();
    }

    pub fn is_open(&self) -> bool {
        self.dialog.is_open()
    }

    /// Where it stacks among the message dialogs ([`MessageDialog::raised`]).
    pub fn raised(&self) -> u64 {
        self.dialog.raised()
    }

    pub fn advance_animations(&mut self) {
        self.dialog.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.dialog.are_animations_ongoing()
    }

    /// Feed a key: Esc cancels, Enter activates the focused button, Left/Right/Tab move focus, and
    /// everything else is swallowed while open.
    pub fn handle_key(&mut self, raw: Option<Keysym>, pressed: bool) -> DialogOutcome {
        self.dialog.handle_key(raw, pressed).into()
    }

    /// Pointer moved to `pos` (output-local logical). Hovering a button focuses it; returns whether
    /// focus changed.
    pub fn pointer_motion(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.dialog.pointer_motion(output_size, pos)
    }

    /// Left click at `pos` (output-local logical).
    pub fn pointer_click(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> DialogOutcome {
        self.dialog.pointer_click(output_size, pos).into()
    }

    /// The Cancel button, output-local, where a click on it lands; empty before the dialog is
    /// first shown.
    pub fn cancel_button_rect(&self, output_size: Size<f64, Logical>) -> Rectangle<f64, Logical> {
        self.dialog
            .button_rect(output_size, CANCEL)
            .unwrap_or_default()
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        output: &Output,
        accent: [u8; 3],
        push: &mut dyn FnMut(EndSessionDialogRenderElement),
    ) {
        self.dialog.render(renderer, icons, output, accent, push);
    }
}

/// The dialog for a presentation, its countdown and its update checkbox.
fn dialog_content(
    presentation: Presentation,
    seconds_left: Option<u64>,
    checkbox: Option<bool>,
) -> DialogContent {
    let (title, action, description) =
        content(presentation, seconds_left, checkbox.unwrap_or(false));
    // **Divergence:** GNOME marks no button default, so its focus starts on Cancel; ours starts on
    // the action, so Enter confirms.
    let buttons = vec![
        DialogButton::new("Cancel").key(Keysym::Escape),
        DialogButton::new(action).default(),
    ];
    DialogContent {
        title: title.to_owned(),
        description,
        check: checkbox.map(|checked| DialogCheck {
            label: CHECK_LABEL.to_owned(),
            checked,
        }),
        buttons,
        width_em: Some(WIDTH_EM),
    }
}

/// Title, action-button label, and counting-down description for a presentation — mirrors the
/// per-type content objects in gnome-shell's `endSessionDialog.js:44-124`.
///
/// `subject` / `description` / `confirmButtons[0].label`, in that order. The fourth presentation,
/// [`Presentation::UpdateRestart`], is the one gnome-session never asks for
/// (`restartUpdateDialogContent`, `:106-124`).
fn content(
    presentation: Presentation,
    seconds_left: Option<u64>,
    installing_updates: bool,
) -> (&'static str, &'static str, String) {
    // Title, action label, and the two description phrasings: the counting-down clause and the
    // question we ask when `Open` gave us no timeout (which GNOME, always counting down, has no
    // string for).
    let (title, action, counting, question) = match presentation {
        Presentation::Logout => (
            "Log Out",
            "Log Out",
            "You will be logged out automatically",
            "log out",
        ),
        Presentation::Shutdown => (
            "Power Off",
            "Power Off",
            "The system will power off automatically",
            "power off",
        ),
        Presentation::Restart => (
            "Restart",
            "Restart",
            "The system will restart automatically",
            "restart",
        ),
        Presentation::UpdateRestart => (
            "Restart & Install Updates",
            "Restart & Install",
            "The system will automatically restart and install updates",
            "restart and install updates",
        ),
    };
    // `subjectWithUpdates` (`js/ui/endSessionDialog.js:70`/`:89`): ticking the box renames the
    // dialog, because it is now describing two things happening, not one. Only the *title*
    // changes — the button keeps its own label. `UpdateRestart` carries no box, so it never
    // reaches this.
    let title = match (presentation, installing_updates) {
        (Presentation::Shutdown, true) => "Install Updates & Power Off",
        (Presentation::Restart, true) => "Install Updates & Restart",
        _ => title,
    };
    let description = match seconds_left {
        // GNOME uses ngettext here; the singular is the last tick of every countdown, so it is a
        // second the user reliably sees.
        Some(1) => format!("{counting} in 1 second."),
        Some(secs) => format!("{counting} in {secs} seconds."),
        None => format!("Do you want to {question}?"),
    };
    (title, action, description)
}

pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label("End Session");
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our buttons share the row, where GNOME's do too, but a label can still outgrow its half.
    /// Hold every action label to the button it is drawn in, so the next long string fails here
    /// rather than on screen.
    #[test]
    #[cfg_attr(
        not(feature = "reference-env"),
        ignore = "measures shaped text, so it needs the reference font stack; \
run it with --features reference-env, as the fedora CI job does"
    )]
    fn the_action_label_fits_its_button() {
        let output = Size::from((1920., 1080.));
        for presentation in [
            Presentation::Logout,
            Presentation::Shutdown,
            Presentation::Restart,
            Presentation::UpdateRestart,
        ] {
            let mut dialog = EndSessionDialog::new(Clock::with_time(Default::default()));
            dialog.show(presentation);
            let (_, label, _) = content(presentation, Some(59), false);
            let width = synoik_vk::text::measure_line_width_weighted(
                label,
                crate::ui::pt_to_px(11.) as f32,
                true,
            );
            // `%dialog_button { padding: $base_padding * 2 }` (`_common.scss:211-213`): 12px
            // either side of the label.
            let button = dialog.dialog.button_rect(output, ACTION).unwrap();
            let available = button.size.w - 2. * 12.;
            assert!(
                width <= available,
                "{presentation:?} label {label:?} is {width}px, but only {available}px fits",
            );
        }
    }
}
