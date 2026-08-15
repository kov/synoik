// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::ffi::OsString;

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use synoik_ipc::{Action, InjectedEvent, OutputAction};

use crate::utils::version;

#[derive(Parser)]
#[command(author, version = version(), about, long_about = None)]
#[command(args_conflicts_with_subcommands = true)]
#[command(subcommand_value_name = "SUBCOMMAND")]
#[command(subcommand_help_heading = "Subcommands")]
pub struct Cli {
    /// Import environment globally to systemd and D-Bus, run D-Bus services.
    ///
    /// Set this flag in a systemd service started by your display manager, or when running
    /// manually as your main compositor instance. Do not set when running as a nested window, or
    /// on a TTY as your non-main compositor instance, to avoid messing up the global environment.
    #[arg(long)]
    pub session: bool,
    /// Run with a headless backend: no display or input devices, virtual
    /// outputs only (see `--output`).
    ///
    /// The compositor is fully driveable over IPC (`synoik msg`), which is the
    /// point: it allows exercising the real compositor — spawning clients,
    /// invoking actions, watching the event stream — without a Wayland
    /// session or a free VT.
    #[arg(long)]
    pub headless: bool,
    /// Virtual output for `--headless`, as `WIDTHxHEIGHT[@SCALE]` (e.g. `1600x1000@1.25`).
    ///
    /// Repeat the flag for more than one output; they are laid out left to right in the order
    /// given. Defaults to a single 1920x1080. Omitting `@SCALE` leaves the scale to the usual
    /// precedence (monitors.xml, then the DPI guess) rather than pinning it to 1.
    ///
    /// Chrome that adapts to the canvas has to be judged on a canvas, so this is how a headless
    /// run reproduces the display it is standing in for — and two outputs at different scales is
    /// the only way to test a window crossing a scale boundary.
    #[arg(long = "output", value_name = "WxH[@SCALE]", requires = "headless")]
    pub outputs: Vec<HeadlessOutput>,
    /// Listen on this Wayland socket name instead of the first free `wayland-N`.
    ///
    /// Fails to start if the name is already taken, rather than quietly landing somewhere else.
    /// The socket is created in `XDG_RUNTIME_DIR` as usual, so a test rig that sets both knows
    /// exactly where to point its clients without scraping the log for the name we chose.
    #[arg(long, value_name = "NAME")]
    pub wayland_display: Option<String>,
    /// Command to run upon compositor startup.
    #[arg(last = true)]
    pub command: Vec<OsString>,

    #[command(subcommand)]
    pub subcommand: Option<Sub>,
}

/// A `--output WIDTHxHEIGHT[@SCALE]` virtual output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadlessOutput {
    pub width: u16,
    pub height: u16,
    /// `None` leaves the scale to the usual precedence (monitors.xml, then the DPI guess).
    pub scale: Option<f64>,
}

impl std::str::FromStr for HeadlessOutput {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (size, scale) = match s.split_once('@') {
            Some((size, scale)) => {
                let scale: f64 = scale
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid scale in {s:?}, expected a number"))?;
                if !(scale.is_finite() && scale > 0.) {
                    return Err(format!(
                        "invalid scale in {s:?}, expected a positive number"
                    ));
                }
                (size, Some(scale))
            }
            None => (s, None),
        };

        let (w, h) = size
            .split_once(['x', 'X'])
            .ok_or_else(|| format!("invalid output {s:?}, expected WIDTHxHEIGHT[@SCALE]"))?;
        let parse = |v: &str, what: &str| -> Result<u16, String> {
            match v.trim().parse::<u16>() {
                Ok(v) if v > 0 => Ok(v),
                _ => Err(format!("invalid {what} in {s:?}, expected 1..=65535")),
            }
        };

        Ok(Self {
            width: parse(w, "width")?,
            height: parse(h, "height")?,
            scale,
        })
    }
}

#[derive(Subcommand)]
pub enum Sub {
    /// Communicate with the running synoik instance.
    Msg {
        #[command(subcommand)]
        msg: Msg,
        /// Format output as JSON.
        #[arg(short, long)]
        json: bool,
    },
    /// Cause a panic to check if the backtraces are good.
    Panic,
    /// Generate shell completions.
    Completions { shell: CompletionShell },
}

#[derive(Subcommand)]
pub enum Msg {
    /// List connected outputs.
    Outputs,
    /// List workspaces.
    Workspaces,
    /// List open windows.
    Windows,
    /// List open layer-shell surfaces.
    Layers,
    /// Get the configured keyboard layouts.
    KeyboardLayouts,
    /// Print information about the focused output.
    FocusedOutput,
    /// Print information about the focused window.
    FocusedWindow,
    /// Pick a window with the mouse and print information about it.
    PickWindow,
    /// Pick a color from the screen with the mouse.
    PickColor,
    /// Perform an action.
    Action {
        #[command(subcommand)]
        action: Action,
    },
    /// Change output configuration temporarily.
    ///
    /// The configuration is changed temporarily and not saved into the config file. If the output
    /// configuration subsequently changes in the config file, these temporary changes will be
    /// forgotten.
    Output {
        /// Output name.
        ///
        /// Run `synoik msg outputs` to see the output names.
        #[arg()]
        output: String,
        /// Configuration to apply.
        #[command(subcommand)]
        action: OutputAction,
    },
    /// Inject synthetic input events through the real input pipeline.
    ///
    /// The events behave exactly like hardware input: binds, grabs and focus
    /// all react as if the keys were physically pressed. Mainly useful for
    /// driving a `--headless` instance, which has no input devices.
    Input {
        /// The input event to inject.
        #[command(subcommand)]
        input: InputCmd,
    },
    /// Start continuously receiving events from the compositor.
    EventStream,
    /// Print the version of the running synoik instance.
    Version,
    /// Request an error from the running synoik instance.
    RequestError,
    /// Print the overview state.
    OverviewState,
    /// Print the live overview, keyboard-focus and input-method state, for debugging.
    DebugFocusState,
    /// List screencasts.
    Casts,
    /// Print this session's frame-timing tallies.
    FramePerf,
}

/// One `synoik msg input` invocation, turned into a batch of [`InjectedEvent`]s.
///
/// Keys are XKB keysym names (case-insensitive: `Super_L`, `F2`, `a`, `8`), resolved through
/// the running instance's active keymap, or `code:N` for a raw decimal evdev keycode
/// (`code:125` is `KEY_LEFTMETA`). Buttons are `left`, `middle`, `right`, or an evdev `BTN_*`
/// code in decimal.
///
/// A bare number is the **digit key**: `Super+8` presses `8`, as the accelerator string reads.
/// It used to mean the keycode, where evdev 8 is `KEY_7`, so every digit accelerator quietly
/// fired its neighbour.
#[derive(Subcommand)]
pub enum InputCmd {
    /// Tap a key or a combo: press, then release in reverse order.
    ///
    /// A combo is keys joined with `+`, e.g. `Alt+F2` or `Super_L+Tab`.
    Key {
        /// The key or `+`-joined combo to tap.
        combo: String,
    },
    /// Press and hold a key (release it later with key-release).
    KeyPress {
        /// The key to press.
        key: String,
    },
    /// Release a held key.
    KeyRelease {
        /// The key to release.
        key: String,
    },
    /// Type a string of text.
    Text {
        /// The text to type.
        text: String,
    },
    /// Move the pointer by a relative delta in logical pixels.
    PointerMotion {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Click: press, then release a pointer button.
    Click {
        /// The button to click.
        #[arg(default_value = "left")]
        button: String,
    },
    /// Press and hold a pointer button (release it later with button-release).
    ButtonPress {
        /// The button to press.
        button: String,
    },
    /// Release a held pointer button.
    ButtonRelease {
        /// The button to release.
        button: String,
    },
    /// Scroll the vertical wheel by whole notches (positive scrolls down).
    Scroll {
        /// The number of notches.
        notches: f64,
    },
}

impl InputCmd {
    /// The injection events this command stands for, in order.
    pub fn to_events(&self) -> Vec<InjectedEvent> {
        match self {
            InputCmd::Key { combo } => {
                let keys: Vec<&str> = combo.split('+').collect();
                let presses = keys.iter().map(|key| InjectedEvent::KeyPress {
                    key: (*key).to_owned(),
                });
                let releases = keys.iter().rev().map(|key| InjectedEvent::KeyRelease {
                    key: (*key).to_owned(),
                });
                presses.chain(releases).collect()
            }
            InputCmd::KeyPress { key } => vec![InjectedEvent::KeyPress { key: key.clone() }],
            InputCmd::KeyRelease { key } => vec![InjectedEvent::KeyRelease { key: key.clone() }],
            InputCmd::Text { text } => vec![InjectedEvent::Text { text: text.clone() }],
            InputCmd::PointerMotion { dx, dy } => {
                vec![InjectedEvent::PointerMotion { dx: *dx, dy: *dy }]
            }
            InputCmd::Click { button } => vec![
                InjectedEvent::ButtonPress {
                    button: button.clone(),
                },
                InjectedEvent::ButtonRelease {
                    button: button.clone(),
                },
            ],
            InputCmd::ButtonPress { button } => vec![InjectedEvent::ButtonPress {
                button: button.clone(),
            }],
            InputCmd::ButtonRelease { button } => vec![InjectedEvent::ButtonRelease {
                button: button.clone(),
            }],
            InputCmd::Scroll { notches } => vec![InjectedEvent::Scroll { notches: *notches }],
        }
    }
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
    Nushell,
}

impl TryFrom<CompletionShell> for Shell {
    type Error = &'static str;

    fn try_from(shell: CompletionShell) -> Result<Self, Self::Error> {
        match shell {
            CompletionShell::Bash => Ok(Shell::Bash),
            CompletionShell::Elvish => Ok(Shell::Elvish),
            CompletionShell::Fish => Ok(Shell::Fish),
            CompletionShell::PowerShell => Ok(Shell::PowerShell),
            CompletionShell::Zsh => Ok(Shell::Zsh),
            CompletionShell::Nushell => Err("Nushell should be handled separately"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    #[test]
    fn headless_output_parses_size_and_scale() {
        assert_eq!(
            HeadlessOutput::from_str("1600x1000@1.25").unwrap(),
            HeadlessOutput {
                width: 1600,
                height: 1000,
                scale: Some(1.25),
            }
        );
        // No scale leaves it to the usual precedence rather than pinning 1.0 — pinning would make
        // the flag silently override monitors.xml for anyone who only wanted a size.
        assert_eq!(
            HeadlessOutput::from_str("800x600").unwrap(),
            HeadlessOutput {
                width: 800,
                height: 600,
                scale: None,
            }
        );
    }

    #[test]
    fn headless_output_rejects_what_it_cannot_honor() {
        // A zero dimension or a zero/negative scale would reach the compositor as a mode or scale
        // no output can have; refuse at the boundary, where the message can name the argument.
        for bad in [
            "",
            "1600",
            "x600",
            "1600x",
            "0x600",
            "1600x0",
            "1600x1000@0",
            "1600x1000@-1",
            "1600x1000@x",
            "99999x1000",
        ] {
            assert!(
                HeadlessOutput::from_str(bad).is_err(),
                "{bad:?} must not parse",
            );
        }
    }
}
