use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use niri_ipc::{Action, InjectedEvent, OutputAction};

use crate::utils::version;

#[derive(Parser)]
#[command(author, version = version(), about, long_about = None)]
#[command(args_conflicts_with_subcommands = true)]
#[command(subcommand_value_name = "SUBCOMMAND")]
#[command(subcommand_help_heading = "Subcommands")]
pub struct Cli {
    /// Path to config file (default: `$XDG_CONFIG_HOME/niri/config.kdl`).
    ///
    /// This can also be set with the `NIRI_CONFIG` environment variable. If both are set, the
    /// command line argument takes precedence.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Import environment globally to systemd and D-Bus, run D-Bus services.
    ///
    /// Set this flag in a systemd service started by your display manager, or when running
    /// manually as your main compositor instance. Do not set when running as a nested window, or
    /// on a TTY as your non-main compositor instance, to avoid messing up the global environment.
    #[arg(long)]
    pub session: bool,
    /// Run with a headless backend: no display or input devices, one virtual
    /// output.
    ///
    /// The compositor is fully driveable over IPC (`niri msg`), which is the
    /// point: it allows exercising the real compositor — spawning clients,
    /// invoking actions, watching the event stream — without a Wayland
    /// session or a free VT.
    #[arg(long)]
    pub headless: bool,
    /// Renderer to draw with: `gles` (default) or `vulkan`.
    ///
    /// `vulkan` selects the fork's experimental owned Vulkan renderer; it requires a build with
    /// the `vulkan` feature and is currently only supported on the headless backend. This can also
    /// be set with the `NIRI_RENDERER` environment variable; the command-line argument takes
    /// precedence.
    #[arg(long, value_enum)]
    pub renderer: Option<crate::backend::RendererKind>,
    /// Command to run upon compositor startup.
    #[arg(last = true)]
    pub command: Vec<OsString>,

    #[command(subcommand)]
    pub subcommand: Option<Sub>,
}

#[derive(Subcommand)]
pub enum Sub {
    /// Communicate with the running niri instance.
    Msg {
        #[command(subcommand)]
        msg: Msg,
        /// Format output as JSON.
        #[arg(short, long)]
        json: bool,
    },
    /// Validate the config file.
    Validate {
        /// Path to config file (default: `$XDG_CONFIG_HOME/niri/config.kdl`).
        ///
        /// This can also be set with the `NIRI_CONFIG` environment variable. If both are set, the
        /// command line argument takes precedence.
        #[arg(short, long)]
        config: Option<PathBuf>,
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
        /// Run `niri msg outputs` to see the output names.
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
    /// Print the version of the running niri instance.
    Version,
    /// Request an error from the running niri instance.
    RequestError,
    /// Print the overview state.
    OverviewState,
    /// List screencasts.
    Casts,
}

/// One `niri msg input` invocation, turned into a batch of [`InjectedEvent`]s.
///
/// Keys are Linux evdev keycodes in decimal (e.g. `125` for `KEY_LEFTMETA`)
/// or XKB keysym names (case-insensitive: `Super_L`, `F2`, `a`), resolved
/// through the running instance's active keymap. Buttons are `left`,
/// `middle`, `right`, or an evdev `BTN_*` code in decimal.
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
