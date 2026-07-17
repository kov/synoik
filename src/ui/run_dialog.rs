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
use niri_vk::text::{SpanFamily, TextSpan};
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::niri_render_elements;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

/// Padding around the dialog text block, logical px.
const PADDING: i32 = 16;
/// Wrap width of the dialog text area, logical px.
const WIDTH: i32 = 400;
/// Base dialog font size (title + entry), logical px-per-em.
const BASE_FONT_PX: f64 = 14.;
/// Small (description/hint) font size, logical px-per-em.
const SMALL_FONT_PX: f64 = 11.;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// Dialog box background (opaque dark grey), straight RGBA.
const BOX_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
/// Dialog text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];
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
    cache: RefCell<DialogCache>,
}

/// Cached dialog box textures per output scale, tagged with the content
/// revision they were rendered at. Tied to a renderer context: dropped
/// wholesale when the renderer changes.
struct DialogCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl DialogCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
        }
    }
}

niri_render_elements! {
    RunDialogRenderElement => {
        // The dialog box, drawn offscreen on the GPU (dark box + one glyph paragraph).
        Texture = TextureRenderElement<VkTexture>,
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
            cache: RefCell::new(DialogCache::new()),
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

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        push: &mut dyn FnMut(RunDialogRenderElement),
    ) {
        if !self.open {
            return;
        }
        let _span = tracy_client::span!("RunDialog::render");

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);
        let Some(scale_key) = NotNan::new(scale).ok() else {
            return;
        };

        let texture = {
            let mut cache = self.cache.borrow_mut();

            // The cached textures belong to one renderer context; drop them all if it changed.
            let context = renderer.context_id();
            if cache.context.as_ref() != Some(&context) {
                cache.textures.clear();
                cache.context = Some(context);
            }

            let fresh =
                matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == self.revision);
            if !fresh {
                match draw_dialog_texture(renderer, scale, &self.entry, self.error.as_deref()) {
                    Ok(texture) => {
                        cache.textures.insert(scale_key, (self.revision, texture));
                    }
                    Err(err) => {
                        warn!("error rendering the run dialog: {err:#}");
                        return;
                    }
                }
            }
            match cache.textures.get(&scale_key) {
                Some((_, texture)) => texture.clone(),
                None => return,
            }
        };

        // The box is opaque, so let the compositor skip drawing behind it.
        let tex_size = texture.size();
        let opaque = vec![Rectangle::from_size(tex_size)];
        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, opaque);

        let size = Size::<f64, Logical>::from((
            f64::from(tex_size.w) / scale,
            f64::from(tex_size.h) / scale,
        ));
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
        push(RunDialogRenderElement::Texture(elem));

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

/// Draw the dialog box into an offscreen [`VkTexture`] on the GPU: clear the
/// opaque dark background, then draw one center-aligned glyph paragraph — a bold
/// "Run a Command" title, the monospace entry with a cursor bar, and a small
/// description/hint line. The returned texture is `SHADER_READ_ONLY`
/// (sampleable) so the caller composites it directly. No cairo/pango raster.
///
/// Unlike the old pango path (which ellipsized a too-long entry from the start),
/// a long command now *wraps* to more lines and the box grows — acceptable, and
/// keeps every glyph visible.
fn draw_dialog_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    entry: &str,
    error: Option<&str>,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("run_dialog::draw_dialog_texture");

    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let padding = padding.max(0);
    let wrap_px: i32 = to_physical_precise_round(scale, WIDTH);
    let wrap_px = wrap_px.max(1);

    let base_px = (BASE_FONT_PX * scale) as f32;
    let small_px = (SMALL_FONT_PX * scale) as f32;

    let description = error.unwrap_or("Press ESC to close");
    // The entry line carries a trailing cursor bar (U+258F), like gnome-shell.
    let entry_line = format!("{entry}\u{258f}");
    let spans = [
        TextSpan {
            text: "Run a Command",
            family: SpanFamily::Sans,
            bold: true,
            px: base_px,
        },
        TextSpan {
            text: "\n\n",
            family: SpanFamily::Sans,
            bold: false,
            px: base_px,
        },
        TextSpan {
            text: &entry_line,
            family: SpanFamily::Mono,
            bold: false,
            px: base_px,
        },
        TextSpan {
            text: "\n\n",
            family: SpanFamily::Sans,
            bold: false,
            px: small_px,
        },
        TextSpan {
            text: description,
            family: SpanFamily::Sans,
            bold: false,
            px: small_px,
        },
    ];

    let run = renderer.build_glyph_paragraph(&spans, wrap_px as f32, base_px)?;

    // The paragraph is laid out in a [0, wrap_px] frame; size the box to its ink
    // plus padding and place the block at (padding, padding) (keeping the
    // per-line centering intact).
    let (_ix, iy, _iw, ih) = run.ink_bounds();
    let text_h = ih.max(1);
    let box_w = (wrap_px + padding * 2).max(1);
    let box_h = (text_h + padding * 2).max(1);
    let origin = Point::<i32, Physical>::from((padding, padding - iy));

    let size = Size::<i32, Physical>::from((box_w, box_h));
    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((box_w, box_h)),
    )?;

    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;
        let full = Rectangle::from_size(size);

        frame.clear(Color32F::from(BOX_BG), &[full])?;
        frame.render_glyphs(&run, origin, TEXT, full, &[full])?;
        // finish() submits and fence-waits synchronously, so the sync point is already signaled.
        let _sync = frame.finish()?;
    }

    // The box is sampled by its own render element; transition it to shader-read.
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
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

    /// Drive the GPU dialog box into an offscreen and read it back: it must have
    /// an opaque dark background and bright glyph ink (the title/entry/hint), for
    /// arbitrary entry/error text — including former markup metacharacters, which
    /// are now just literal glyphs (there is no markup parser to escape). Skips
    /// cleanly with no Vulkan device.
    #[test]
    fn draws_the_dialog_with_glyph_coverage() {
        use smithay::backend::renderer::ExportMem;
        use smithay::utils::Rectangle;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_the_dialog_with_glyph_coverage: no Vulkan device ({e})");
                return;
            }
        };

        for (entry, error) in [
            ("", None),
            ("echo <b>hi</b> & 'quotes'", None),
            ("cat<", Some("error with <markup> & entities")),
        ] {
            let mut tex = draw_dialog_texture(&mut vk, 1., entry, error).expect("dialog texture");
            let size = tex.size();
            assert!(size.w > 0 && size.h > 0);

            let fb = vk.bind(&mut tex).expect("bind for readback");
            let region = Rectangle::<i32, BufferCoord>::from_size(size);
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

            // A pixel near the bottom-right corner (past any text) is the opaque dark box.
            let bx = size.w - 3;
            let by = size.h - 3;
            let i = ((by * size.w + bx) * 4) as usize;
            let bg = [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]];
            assert_eq!(bg[3], 255, "the box must be opaque, got {bg:?}");
            assert!(
                bg[0] < 60 && bg[1] < 60 && bg[2] < 60,
                "box bg not dark: {bg:?}"
            );

            // Bright glyph ink somewhere (the title, at least).
            let bright = pixels
                .chunks_exact(4)
                .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
                .count();
            assert!(
                bright > 40,
                "expected visible glyph ink for {entry:?}, got {bright}"
            );
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
