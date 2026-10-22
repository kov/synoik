// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The confirmation a keyboard-accessibility shortcut raises — gnome-shell's `KbdA11yDialog`
//! (`js/ui/kbdA11yDialog.js`). Shift pressed five times (Sticky Keys), Shift held eight seconds
//! (Slow Keys) or two modifiers at once under `stickykeys-two-key-off` switch a feature, and this
//! says so and offers to undo it.
//!
//! Two buttons: one keeps the feature on, the other off. The one that leaves things as the
//! shortcut set them is the default (Enter); the one that undoes it answers Escape.

use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::animation::Clock;
use crate::input::keyboard_a11y::KeyboardA11yFeature;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::ui::message_dialog::{
    DialogButton, DialogContent, DialogOutcome, MessageDialog, MessageDialogRenderElement,
};

/// The button that turns the feature on (`Leave On` / `Turn On`), then the one that turns it off.
const ON: usize = 0;
const OFF: usize = 1;

pub struct KbdA11yDialog {
    dialog: MessageDialog,
    /// The feature the dialog is about.
    feature: KeyboardA11yFeature,
}

impl KbdA11yDialog {
    pub fn new(clock: Clock) -> Self {
        Self {
            dialog: MessageDialog::new(clock),
            feature: KeyboardA11yFeature::StickyKeys,
        }
    }

    /// A shortcut just turned `feature` `enabled` (`_showKbdA11yDialog`).
    pub fn show(&mut self, feature: KeyboardA11yFeature, enabled: bool) {
        self.feature = feature;
        self.dialog.show(content(feature, enabled));
    }

    pub fn hide(&mut self) {
        self.dialog.hide();
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

    /// Feed a key: the answer, `(feature, on)`, if it chose one.
    pub fn handle_key(
        &mut self,
        raw: Option<Keysym>,
        pressed: bool,
    ) -> Option<(KeyboardA11yFeature, bool)> {
        let outcome = self.dialog.handle_key(raw, pressed);
        self.answer(outcome)
    }

    /// Pointer moved; returns whether focus changed.
    pub fn pointer_motion(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> bool {
        self.dialog.pointer_motion(output_size, pos)
    }

    /// Left click: the answer, if it chose one.
    pub fn pointer_click(
        &mut self,
        output_size: Size<f64, Logical>,
        pos: Point<f64, Logical>,
    ) -> Option<(KeyboardA11yFeature, bool)> {
        let outcome = self.dialog.pointer_click(output_size, pos);
        self.answer(outcome)
    }

    fn answer(&self, outcome: DialogOutcome) -> Option<(KeyboardA11yFeature, bool)> {
        match outcome {
            DialogOutcome::Button(ON) => Some((self.feature, true)),
            DialogOutcome::Button(OFF) => Some((self.feature, false)),
            _ => None,
        }
    }

    /// Button `i` (0 turns the feature on, 1 off), output-local.
    pub fn button_rect(
        &self,
        output_size: Size<f64, Logical>,
        i: usize,
    ) -> Option<Rectangle<f64, Logical>> {
        self.dialog.button_rect(output_size, i)
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &crate::render_helpers::icon::IconCache,
        output: &Output,
        accent: [u8; 3],
        push: &mut dyn FnMut(MessageDialogRenderElement),
    ) {
        self.dialog.render(renderer, icons, output, accent, push);
    }
}

/// The words and buttons for `feature` just turned `enabled` (`kbdA11yDialog.js:25-81`).
fn content(feature: KeyboardA11yFeature, enabled: bool) -> DialogContent {
    let (title, description) = match (feature, enabled) {
        (KeyboardA11yFeature::SlowKeys, on) => (
            if on {
                "Slow Keys Turned On"
            } else {
                "Slow Keys Turned Off"
            },
            "You just held down the Shift key for 8 seconds. This is the shortcut for the Slow \
             Keys feature, which affects the way your keyboard works.",
        ),
        (KeyboardA11yFeature::StickyKeys, true) => (
            "Sticky Keys Turned On",
            "You just pressed the Shift key 5 times in a row. This is the shortcut for the Sticky \
             Keys feature, which affects the way your keyboard works.",
        ),
        (KeyboardA11yFeature::StickyKeys, false) => (
            "Sticky Keys Turned Off",
            "You just pressed two keys at once, or pressed the Shift key 5 times in a row. This \
             turns off the Sticky Keys feature, which affects the way your keyboard works.",
        ),
    };

    let mut on = DialogButton::new(if enabled { "Leave On" } else { "Turn On" });
    let mut off = DialogButton::new(if enabled { "Turn Off" } else { "Leave Off" });
    if enabled {
        on = on.default();
        off = off.key(Keysym::Escape);
    } else {
        on = on.key(Keysym::Escape);
        off = off.default();
    }
    DialogContent {
        title: title.to_owned(),
        description: description.to_owned(),
        check: None,
        buttons: vec![on, off],
        width_em: None,
    }
}

pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::AlertDialog);
    node.set_label("Keyboard Accessibility");
    node.set_modal();
    node
}
