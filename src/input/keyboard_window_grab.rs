// SPDX-License-Identifier: GPL-3.0-only

//! The keyboard-driven move and resize grabs.
//!
//! mutter's `META_GRAB_OP_KEYBOARD_MOVING` and `META_GRAB_OP_KEYBOARD_RESIZING_*`, driven by
//! `process_keyboard_move_grab` and `process_keyboard_resize_grab`
//! (`meta-window-drag.c:614-1070`). Both are a *virtual pointer*: the arrows walk a delta, and the
//! layout's own interactive move and resize see the same numbers a real drag would give them.
//!
//! This type is only the state machine — it never touches the layout, so the whole key protocol is
//! testable without a compositor.

use smithay::desktop::Window;
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::utils::{Logical, Point};

use crate::utils::ResizeEdge;
use crate::window::mapped::MappedId;

/// mutter's `NORMAL_INCREMENT`.
const NORMAL_INCREMENT: f64 = 10.;
/// mutter's `SMALL_INCREMENT`, and the step Shift's snap mode uses.
const SMALL_INCREMENT: f64 = 1.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabKind {
    Move,
    Resize,
}

/// What the compositor must do with a key the grab was handed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GrabKey {
    /// Eaten, grab kept, nothing to do: a release, or a modifier press.
    Ignore,
    /// The virtual pointer moved. A move nudges by `step`; a resize feeds the whole
    /// [`KeyboardWindowGrab::delta`].
    Update { step: Point<f64, Logical> },
    /// The resize picked a new edge: restart the interactive resize there. The delta has been
    /// reset, and no resizing happens on this key — mutter's op change consumes the press
    /// (`process_keyboard_resize_grab_op_change`).
    ReEdge(ResizeEdge),
    /// Escape: put the window back where it started, then end.
    Cancel,
    /// Any other key: commit where the window is and end. The key is consumed either way.
    Commit,
}

#[derive(Debug)]
pub struct KeyboardWindowGrab {
    pub window: Window,
    /// The same window, in the id every removal path speaks.
    pub id: MappedId,
    pub kind: GrabKind,
    /// The edge being dragged. `None` is mutter's `RESIZING_UNKNOWN` — the first arrow picks one
    /// and resizes nothing. Always `None` for a move.
    pub edges: Option<ResizeEdge>,
    /// How far the virtual pointer has travelled since the grab began, or since the last edge
    /// change. Screen directions, so it reads the same for either kind: the layout's resize
    /// negates it itself for the top and left edges.
    pub delta: Point<f64, Logical>,
}

impl KeyboardWindowGrab {
    pub fn new(window: Window, id: MappedId, kind: GrabKind) -> Self {
        Self {
            window,
            id,
            kind,
            edges: None,
            delta: Point::from((0., 0.)),
        }
    }

    pub fn handle_key(&mut self, keysym: Keysym, mods: &ModifiersState, pressed: bool) -> GrabKey {
        // Releases are eaten but keep the grab, and so are modifier presses — otherwise reaching
        // for Ctrl to slow the step down would end the drag.
        if !pressed || is_modifier(keysym) {
            return GrabKey::Ignore;
        }

        if keysym == Keysym::Escape {
            return GrabKey::Cancel;
        }

        let Some((dx, dy)) = arrow(keysym, self.kind) else {
            return GrabKey::Commit;
        };

        // Shift is mutter's snap mode, which also steps by one.
        let incr = if mods.shift || mods.ctrl {
            SMALL_INCREMENT
        } else {
            NORMAL_INCREMENT
        };
        let step = Point::from((dx * incr, dy * incr));

        if self.kind == GrabKind::Resize {
            // An arrow across the current edge's axis moves the grab to that edge instead of
            // resizing: from unknown it picks the first one, and from a horizontal edge a vertical
            // arrow switches to the vertical one, and back.
            let across = match self.edges {
                None => true,
                Some(e) if e.intersects(ResizeEdge::TOP_BOTTOM) => dx != 0.,
                Some(_) => dy != 0.,
            };
            if across {
                let edge = if dy < 0. {
                    ResizeEdge::TOP
                } else if dy > 0. {
                    ResizeEdge::BOTTOM
                } else if dx < 0. {
                    ResizeEdge::LEFT
                } else {
                    ResizeEdge::RIGHT
                };
                self.edges = Some(edge);
                self.delta = Point::from((0., 0.));
                return GrabKey::ReEdge(edge);
            }
        }

        self.delta += step;
        GrabKey::Update { step }
    }
}

/// The arrow keys, in steps. Only a move takes the keypad's diagonals: mutter's resize switch has
/// no cases for them (`meta-window-drag.c:1000-1070`).
fn arrow(keysym: Keysym, kind: GrabKind) -> Option<(f64, f64)> {
    let step = match keysym {
        Keysym::Up | Keysym::KP_Up => (0., -1.),
        Keysym::Down | Keysym::KP_Down => (0., 1.),
        Keysym::Left | Keysym::KP_Left => (-1., 0.),
        Keysym::Right | Keysym::KP_Right => (1., 0.),
        Keysym::KP_Home if kind == GrabKind::Move => (-1., -1.),
        Keysym::KP_Prior if kind == GrabKind::Move => (1., -1.),
        Keysym::KP_End if kind == GrabKind::Move => (-1., 1.),
        Keysym::KP_Next if kind == GrabKind::Move => (1., 1.),
        _ => return None,
    };
    Some(step)
}

/// mutter's `is_modifier` (`meta-window-drag.c`).
fn is_modifier(keysym: Keysym) -> bool {
    matches!(
        keysym,
        Keysym::Shift_L
            | Keysym::Shift_R
            | Keysym::Control_L
            | Keysym::Control_R
            | Keysym::Caps_Lock
            | Keysym::Shift_Lock
            | Keysym::Meta_L
            | Keysym::Meta_R
            | Keysym::Alt_L
            | Keysym::Alt_R
            | Keysym::Super_L
            | Keysym::Super_R
            | Keysym::Hyper_L
            | Keysym::Hyper_R
            | Keysym::Num_Lock
            | Keysym::ISO_Level3_Shift
            | Keysym::Mode_switch
    )
}
