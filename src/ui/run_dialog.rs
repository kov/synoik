//! The GNOME run dialog (Alt+F2).
//!
//! A fork-owned port of gnome-shell's `js/ui/runDialog.js`, minimal core: type
//! a command, Enter tokenizes it (shell quoting, no shell — mutter's
//! `trySpawnCommandLine` semantics) and spawns it with a PATH search; errors
//! show in-dialog and keep it open; Escape (press + release, like gnome-shell)
//! closes; Up/Down walk the history persisted in `org.gnome.shell
//! command-history`. Deferred vs. gnome-shell: Tab completion, Ctrl+Enter
//! (run in terminal), the open-a-file-path fallback, and the internal
//! commands table (`lg`, `rt`, ...).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use gio::glib;
use ordered_float::NotNan;
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{Alignment, EllipsizeMode, FontDescription, SCALE as PANGO_SCALE};
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Point, Transform};

use crate::niri_render_elements;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 16;
const WIDTH: i32 = 400;
const FONT: &str = "sans 14px";
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// gnome-shell's HistoryManager DEFAULT_LIMIT.
const HISTORY_LIMIT: usize = 512;

pub struct RunDialog {
    open: bool,
    entry: String,
    error: Option<String>,
    /// Escape must be both pressed and released on the dialog to close it
    /// (gnome-shell pairs the press and release).
    esc_pressed: bool,
    /// Position in the history while browsing; `None` = at the fresh entry
    /// past the end.
    history_index: Option<usize>,
    /// Bumped on every content change to invalidate rendered buffers.
    revision: u64,
    buffers: RefCell<BuffersByScale>,
}

/// Rendered dialog buffers per output scale, tagged with the content revision
/// they were rendered at.
type BuffersByScale = HashMap<NotNan<f64>, (u64, Option<MemoryBuffer>)>;

niri_render_elements! {
    RunDialogRenderElement => {
        Texture = PrimaryGpuTextureRenderElement,
        SolidColor = SolidColorRenderElement,
    }
}

/// What a key event on the dialog asked for.
pub enum KeyOutcome {
    /// Nothing further; the key was consumed.
    Handled,
    /// Close the dialog.
    Close,
    /// Run the entered command line.
    Run(String),
}

impl RunDialog {
    pub fn new() -> Self {
        Self {
            open: false,
            entry: String::new(),
            error: None,
            esc_pressed: false,
            history_index: None,
            revision: 0,
            buffers: RefCell::new(HashMap::new()),
        }
    }

    /// Open with a cleared entry and the history cursor past the end
    /// (gnome-shell's `open()` resets both).
    pub fn open(&mut self) {
        self.open = true;
        self.entry.clear();
        self.error = None;
        self.esc_pressed = false;
        self.history_index = None;
        self.revision += 1;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Show an error in the description area; the dialog stays open and the
    /// entry keeps its text so the user can fix the command.
    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.revision += 1;
    }

    /// Feed a key on the open dialog. `text` is the key's character, if any;
    /// `history` is the current command history (oldest first).
    pub fn handle_key(
        &mut self,
        raw: Option<smithay::input::keyboard::Keysym>,
        text: Option<char>,
        pressed: bool,
        history: &[String],
    ) -> KeyOutcome {
        use smithay::input::keyboard::Keysym;

        if !pressed {
            if raw == Some(Keysym::Escape) && self.esc_pressed {
                return KeyOutcome::Close;
            }
            return KeyOutcome::Handled;
        }

        self.esc_pressed = raw == Some(Keysym::Escape);

        match raw {
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                return KeyOutcome::Run(self.entry.clone());
            }
            Some(Keysym::Up) => {
                let index = match self.history_index {
                    None => history.len().checked_sub(1),
                    Some(index) => Some(index.saturating_sub(1)),
                };
                if let Some(index) = index {
                    self.history_index = Some(index);
                    self.set_entry(history[index].clone());
                }
            }
            Some(Keysym::Down) => {
                if let Some(index) = self.history_index {
                    if index + 1 < history.len() {
                        self.history_index = Some(index + 1);
                        self.set_entry(history[index + 1].clone());
                    } else {
                        self.history_index = None;
                        self.set_entry(String::new());
                    }
                }
            }
            Some(Keysym::BackSpace) => {
                if self.entry.pop().is_some() {
                    self.entry_edited();
                }
            }
            _ => {
                if let Some(c) = text.filter(|c| !c.is_control()) {
                    self.entry.push(c);
                    self.entry_edited();
                }
            }
        }

        KeyOutcome::Handled
    }

    fn set_entry(&mut self, text: String) {
        self.entry = text;
        self.entry_edited();
    }

    /// Any edit clears a stale error back to the hint text (gnome-shell resets
    /// the description on `notify::text`).
    fn entry_edited(&mut self) {
        self.error = None;
        self.revision += 1;
    }

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
        push: &mut dyn FnMut(RunDialogRenderElement),
    ) {
        if !self.open {
            return;
        }
        let _span = tracy_client::span!("RunDialog::render");

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);

        let mut buffers = self.buffers.borrow_mut();
        let (revision, buffer) = buffers
            .entry(NotNan::new(scale).unwrap())
            .or_insert((u64::MAX, None));
        if *revision != self.revision {
            *buffer = render(scale, &self.entry, self.error.as_deref())
                .map_err(|err| warn!("error rendering the run dialog: {err:?}"))
                .ok();
            *revision = self.revision;
        }
        let Some(buffer) = buffer else {
            return;
        };

        let size = buffer.logical_size();
        // The dialog texture uploads to a GlesTexture; skip drawing it on the owned Vulkan
        // renderer.
        let Some(gles) = renderer.try_as_gles_renderer() else {
            return;
        };
        let Ok(buffer) = TextureBuffer::from_memory_buffer(gles, buffer) else {
            return;
        };

        let location = (output_size.to_point() - size.to_point()).downscale(2.);
        let mut location = location.to_physical_precise_round(scale).to_logical(scale);
        location.x = f64::max(0., location.x);
        location.y = f64::max(0., location.y);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            1.,
            None,
            None,
            Kind::Unspecified,
        );
        push(RunDialogRenderElement::Texture(
            PrimaryGpuTextureRenderElement(elem),
        ));

        // Backdrop.
        let backdrop = SolidColorBuffer::new(output_size, BACKDROP_COLOR);
        let elem = SolidColorRenderElement::from_buffer(
            &backdrop,
            Point::new(0., 0.),
            1.,
            Kind::Unspecified,
        );
        push(RunDialogRenderElement::SolidColor(elem));
    }
}

impl Default for RunDialog {
    fn default() -> Self {
        Self::new()
    }
}

fn render(scale: f64, entry: &str, error: Option<&str>) -> anyhow::Result<MemoryBuffer> {
    let _span = tracy_client::span!("run_dialog::render");

    let markup = markup(entry, error);
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let width: i32 = to_physical_precise_round(scale, WIDTH);

    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let surface = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
    let cr = cairo::Context::new(&surface)?;
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_alignment(Alignment::Center);
    layout.set_width(width * PANGO_SCALE);
    layout.set_ellipsize(EllipsizeMode::Start);
    layout.set_markup(&markup);

    let (_, mut height) = layout.pixel_size();
    height += padding * 2;
    let width = width + padding * 2;

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.set_source_rgb(0.1, 0.1, 0.1);
    cr.paint()?;

    cr.move_to(padding.into(), padding.into());
    let layout = pangocairo::functions::create_layout(&cr);
    layout.context().set_round_glyph_positions(false);
    layout.set_font_description(Some(&font));
    layout.set_alignment(Alignment::Center);
    layout.set_width((width - padding * 2) * PANGO_SCALE);
    layout.set_ellipsize(EllipsizeMode::Start);
    layout.set_markup(&markup);

    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &layout);
    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = MemoryBuffer::new(
        data.to_vec(),
        Fourcc::Argb8888,
        (width, height),
        scale,
        Transform::Normal,
    );

    Ok(buffer)
}

fn markup(entry: &str, error: Option<&str>) -> String {
    let entry = glib::markup_escape_text(entry);
    let description = match error {
        Some(error) => glib::markup_escape_text(error),
        None => glib::markup_escape_text("Press ESC to close"),
    };
    format!(
        "<b>Run a Command</b>\n\n\
         <tt>{entry}\u{258f}</tt>\n\n\
         <small>{description}</small>"
    )
}

/// Tokenize and resolve a run dialog command line the way gnome-shell does:
/// `g_shell_parse_argv` (shell quoting honored, but no pipes/expansion — this
/// is an argv split, not a shell), then a PATH lookup for the executable.
/// gnome-shell maps a failed spawn's NOENT to "Command not found"; we check
/// up front so the error can show without a spawn attempt.
pub fn resolve_command_line(input: &str) -> Result<Vec<String>, String> {
    let argv = glib::shell_parse_argv(input)
        .map_err(|err| err.message().to_owned())?
        .into_iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    match argv.first() {
        None => Err("Command not found".to_owned()),
        Some(exe) if command_exists(exe) => Ok(argv),
        Some(_) => Err("Command not found".to_owned()),
    }
}

fn command_exists(exe: &str) -> bool {
    fn is_executable(path: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }

    if exe.contains('/') {
        return is_executable(Path::new(exe));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(exe)))
}

/// Add a run command to the history, with gnome-shell's `HistoryManager`
/// semantics: trim; skip if empty or identical to the last entry; drop an
/// earlier duplicate; cap the length. Returns the trimmed input (what `_run`
/// executes).
pub fn history_add(history: &mut Vec<String>, input: &str) -> String {
    let input = input.trim().to_owned();
    if input.is_empty() {
        return input;
    }
    if history.last() != Some(&input) {
        history.retain(|entry| *entry != input);
        history.push(input.clone());
    }
    if history.len() > HISTORY_LIMIT {
        let excess = history.len() - HISTORY_LIMIT;
        history.drain(..excess);
    }
    input
}

#[cfg(feature = "dbus")]
pub fn a11y_node() -> accesskit::Node {
    let mut node = accesskit::Node::new(accesskit::Role::Dialog);
    node.set_label("Run a Command");
    node.set_modal();
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_command_line_tokenizes_with_quoting() {
        // /bin/sh always exists; quoting is honored, no shell interpretation.
        assert_eq!(
            resolve_command_line("/bin/sh -c 'echo hi there'"),
            Ok(vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "echo hi there".to_owned(),
            ])
        );
    }

    #[test]
    fn resolve_command_line_searches_path() {
        assert!(resolve_command_line("sh").is_ok());
    }

    #[test]
    fn resolve_command_line_unknown_command() {
        assert_eq!(
            resolve_command_line("definitely-not-a-real-command-xyz"),
            Err("Command not found".to_owned())
        );
    }

    #[test]
    fn resolve_command_line_empty_is_an_error() {
        // gnome-shell lets g_shell_parse_argv fail on empty input and shows
        // the error; there is no special empty-string short-circuit.
        assert!(resolve_command_line("").is_err());
        assert!(resolve_command_line("   ").is_err());
    }

    /// The CPU half of rendering (pango markup + cairo) works for arbitrary
    /// entry/error text — in particular, markup metacharacters in the typed
    /// command must be escaped, not parsed.
    #[test]
    fn render_survives_markup_metacharacters() {
        for (entry, error) in [
            ("", None),
            ("echo <b>hi</b> & 'quotes'", None),
            ("cat<", Some("error with <markup> & entities")),
        ] {
            let buffer = render(1., entry, error).unwrap();
            let size = buffer.logical_size();
            assert!(size.w > 0. && size.h > 0.);
        }
    }

    #[test]
    fn history_add_dedups_and_skips_empty() {
        let mut history = vec!["ls".to_owned(), "true".to_owned()];

        assert_eq!(history_add(&mut history, "  "), "");
        assert_eq!(history, ["ls", "true"]);

        // Same as last: no duplicate appended.
        history_add(&mut history, "true");
        assert_eq!(history, ["ls", "true"]);

        // Earlier duplicate moves to the end.
        history_add(&mut history, " ls ");
        assert_eq!(history, ["true", "ls"]);

        history_add(&mut history, "false");
        assert_eq!(history, ["true", "ls", "false"]);
    }
}
