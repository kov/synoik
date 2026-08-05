// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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
use std::path::Path;

use gio::glib;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Size, Transform};

use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::text_edit::{EditMods, EditOutcome, KeyTheme, TextEdit};
use crate::ui::widget::{self, ContentCache, Painter, ParagraphSpan, ShapedParagraph, TextShaper};
use crate::utils::{output_size, to_physical_precise_round};

/// Padding around the dialog text block, logical px.
const PADDING: i32 = 16;
/// Wrap width of the dialog text area, logical px.
const WIDTH: i32 = 400;
/// Base dialog font size (title + entry), GNOME body, GNOME points.
const BASE_PT: f64 = 11.;
/// Small (description/hint) font size, GNOME `%caption`, GNOME points.
const SMALL_PT: f64 = 9.;
const BACKDROP_COLOR: [f32; 4] = [0., 0., 0., 0.4];
/// Dialog card background — GNOME modal `$bg_color`; rounded, borderless.
const BOX_BG: [f32; 4] = widget::style::DIALOG_BG;
/// Dialog text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// gnome-shell's HistoryManager DEFAULT_LIMIT.
const HISTORY_LIMIT: usize = 512;

pub struct RunDialog {
    open: bool,
    entry: TextEdit,
    error: Option<String>,
    /// Escape must be both pressed and released on the dialog to close it
    /// (gnome-shell pairs the press and release).
    esc_pressed: bool,
    /// Position in the history while browsing; `None` = at the fresh entry
    /// past the end.
    history_index: Option<usize>,
    /// Bumped on every content change to invalidate rendered buffers.
    revision: u64,
    cache: RefCell<ContentCache>,
}

synoik_render_elements! {
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
            entry: TextEdit::new(),
            error: None,
            esc_pressed: false,
            history_index: None,
            revision: 0,
            cache: RefCell::new(ContentCache::new()),
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

    /// Show (or clear) the input method's in-progress composition in the entry.
    pub fn set_preedit(&mut self, preedit: Option<String>) -> bool {
        self.entry.set_preedit(preedit)
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn entry(&self) -> &str {
        self.entry.text()
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
    #[allow(clippy::too_many_arguments)]
    pub fn handle_key(
        &mut self,
        raw: Option<smithay::input::keyboard::Keysym>,
        text: Option<char>,
        mods: EditMods,
        theme: KeyTheme,
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

        // Return and history recall come before the entry.
        //
        // Return is the dialog's whatever the modifiers are: gnome-shell reads the activate
        // event's `CONTROL_MASK` to decide whether to run *in a terminal*
        // (`runDialog.js:113-114`, `_run(input, inTerminal)` `:204,218`), so Ctrl+Enter is a
        // run, not a no-op. Leaving it to `TextEdit` — whose Activate arm is plain-only,
        // because Ctrl+Enter belongs to whoever owns the field — silently swallowed it.
        // (We still always run plainly; the in-terminal half is a separate divergence.)
        //
        // Up/Down are history recall: they are line motion on a one-line field, so the entry
        // has nothing to do with them anyway.
        match raw {
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                return KeyOutcome::Run(self.entry.text().to_owned());
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
            _ => {
                // Everything else is the shared entry's: caret motion, word deletion,
                // selection, `Ctrl-u`/`Ctrl-k`, the Emacs theme.
                let before = self.entry.text().to_owned();
                match self.entry.handle_key(raw, text, mods, theme) {
                    EditOutcome::Changed => {
                        if self.entry.text() != before {
                            self.entry_edited();
                        }
                    }
                    // Return never reaches here (claimed above). The double-Escape close is
                    // handled via `esc_pressed`; a single Escape must not clear the entry the
                    // way the search's does.
                    EditOutcome::Activate
                    | EditOutcome::Moved
                    | EditOutcome::Cancel
                    | EditOutcome::Ignored => {
                        self.revision += 1;
                    }
                }
            }
        }

        KeyOutcome::Handled
    }

    fn set_entry(&mut self, text: String) {
        self.entry.set_text(text);
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

        let texture = {
            let mut cache = self.cache.borrow_mut();
            // This dialog is a modal keyboard grab (`KeyboardFocus::RunDialog`); a
            // draw failure must never leave an invisible trap. On error we skip the
            // box but still draw the backdrop below, so the modal stays visible.
            let entry = self.entry.display(None);
            let entry = entry.as_str();
            let error = self.error.as_deref();
            match widget::bake_content(
                renderer,
                &mut cache,
                scale,
                self.revision,
                |renderer| prepare_dialog(renderer, scale, entry, error),
                |frame, phys, layout| paint_dialog(frame, phys, layout, scale),
            ) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    warn!("error rendering the run dialog: {err:#}");
                    None
                }
            }
        };

        if let Some(texture) = texture {
            // Rounded corners → the card is not fully opaque; no opaque-region hint (its
            // transparent corners must composite over the backdrop).
            let tex_size = texture.size();
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );

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
        }

        // Backdrop — always drawn while the dialog is open, even if the box failed to render, so
        // the modal is never invisible while it holds the keyboard.
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
/// The dialog box layout (fixed width `WIDTH`, content-sized height), produced by
/// [`prepare_dialog`] and drawn by [`paint_dialog`] (the [`widget::bake_content`]
/// phases).
struct DialogLayout {
    run: ShapedParagraph,
    origin: Point<i32, Physical>,
}

/// Shape the title/entry/hint paragraph and size the box — the prepare phase for
/// [`widget::bake_content`]. A long command wraps (the box grows in height) rather
/// than ellipsizing, keeping every glyph visible.
fn prepare_dialog(
    renderer: &mut VulkanRenderer,
    scale: f64,
    entry: &str,
    error: Option<&str>,
) -> anyhow::Result<(Size<i32, Physical>, DialogLayout)> {
    let _span = tracy_client::span!("run_dialog::prepare_dialog");

    let padding = to_physical_precise_round::<i32>(scale, f64::from(PADDING)).max(0);
    let wrap_px = to_physical_precise_round::<i32>(scale, f64::from(WIDTH)).max(1);

    let description = error.unwrap_or("Press ESC to close");
    // The entry line carries a trailing cursor bar (U+258F), like gnome-shell.
    let entry_line = format!("{entry}\u{258f}");
    let spans = [
        ParagraphSpan::new("Run a Command", BASE_PT).bold(),
        ParagraphSpan::new("\n\n", BASE_PT),
        ParagraphSpan::new(&entry_line, BASE_PT).mono(),
        ParagraphSpan::new("\n\n", SMALL_PT),
        ParagraphSpan::new(description, SMALL_PT),
    ];
    let mut shaper = TextShaper::new(renderer, scale);
    let run = shaper.paragraph(&spans, f64::from(WIDTH), BASE_PT)?;

    // The paragraph is laid out in a [0, wrap_px] frame; size the box to its ink
    // plus padding and place the block at (padding, padding) (keeping the
    // per-line centering intact).
    let (_ix, iy, _iw, ih) = run.ink_bounds();
    let text_h = ih.max(1);
    let box_w = (wrap_px + padding * 2).max(1);
    let box_h = (text_h + padding * 2).max(1);
    let origin = Point::<i32, Physical>::from((padding, padding - iy));

    let size = Size::<i32, Physical>::from((box_w, box_h));
    Ok((size, DialogLayout { run, origin }))
}

/// Draw the dark box and the paragraph — the paint phase for
/// [`widget::bake_content`].
fn paint_dialog(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    layout: &DialogLayout,
    scale: f64,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    // Rounded borderless card: transparent clear so the corners composite away, then
    // the flat fill over the whole content-sized buffer.
    p.clear(widget::style::TRANSPARENT)?;
    p.fill_rounded_full(widget::style::DIALOG_RADIUS, BOX_BG)?;
    p.paragraph(&layout.run, layout.origin, TEXT)?;
    Ok(())
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
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{Bind, ExportMem};
        use smithay::utils::{Buffer as BufferCoord, Rectangle};

        use crate::ui::widget::ContentCache;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_the_dialog_with_glyph_coverage: no Vulkan device ({e})");
                return;
            }
        };

        // The last case is the pathological one from review: a long command at
        // scale 2 that overflowed the old fixed 256px glyph atlas — it must draw,
        // not error (an errored draw would leave the modal invisible mid-typing).
        let long_entry = "firefox --new-window https://example.org/a/very/long/path\
                          ?q=alpha+beta+gamma&x=1#anchor && echo done && ls -la /usr/bin"
            .to_owned();
        for (scale, entry, error) in [
            (1., "", None),
            (1., "echo <b>hi</b> & 'quotes'", None),
            (1., "cat<", Some("error with <markup> & entities")),
            (2., long_entry.as_str(), None),
        ] {
            let mut cache = ContentCache::new();
            let mut tex = widget::bake_content(
                &mut vk,
                &mut cache,
                scale,
                0,
                |r| prepare_dialog(r, scale, entry, error),
                |frame, phys, layout| paint_dialog(frame, phys, layout, scale),
            )
            .expect("dialog texture");
            let size = tex.size();
            assert!(size.w > 0 && size.h > 0);

            let fb = vk.bind(&mut tex).expect("bind for readback");
            let region = Rectangle::<i32, BufferCoord>::from_size(size);
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .expect("copy_framebuffer");
            let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

            // The bottom-edge center (past any text, off the rounded corners) is the opaque
            // dark card bg.
            let bx = size.w / 2;
            let by = size.h - 3;
            let i = ((by * size.w + bx) * 4) as usize;
            let bg = [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]];
            assert_eq!(bg[3], 255, "the card must be opaque, got {bg:?}");
            assert!(
                bg[0] < 60 && bg[1] < 60 && bg[2] < 60,
                "card bg not dark: {bg:?}"
            );

            // The extreme corner is transparent — the card is rounded, not a square box.
            let c = ((size.h - 1) * size.w + (size.w - 1)) as usize * 4;
            assert_eq!(
                pixels[c + 3],
                0,
                "card corner should be transparent (rounded)"
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
