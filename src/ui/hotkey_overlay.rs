use std::cell::RefCell;
use std::fmt::Write as _;
use std::rc::Rc;

use niri_config::{Action, Config, Key, ModKey, Modifiers, Trigger};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture;
use smithay::input::keyboard::xkb::keysym_get_name;
use smithay::output::Output;
use smithay::utils::{Physical, Point, Rectangle, Size, Transform};

use crate::gnome::{key_for_accel, GnomeKeybinding};
use crate::input::action_for_keybinding;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::widget::{self, ContentCache, Painter, ParagraphSpan, ShapedParagraph, TextShaper};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 8;
/// Overlay font size, GNOME points. `font_px()` is its logical px, used only for the
/// inline key/spawn patch padding geometry; shaping goes through [`ParagraphSpan`].
const FONT_PT: f64 = 11.;
fn font_px() -> f64 {
    crate::ui::pt_to_px(FONT_PT)
}
const BORDER: i32 = 4;
const LINE_INTERVAL: i32 = 2;
const TITLE: &str = "Important Hotkeys";

/// Dark panel background, light-blue border, the grey patch behind each key, the black patch behind
/// a spawn command, and the (white) text colour.
const PANEL_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
const BORDER_COLOR: [f32; 4] = [0.5, 0.8, 1.0, 1.];
const KEY_BG: [f32; 4] = [0.183, 0.183, 0.183, 1.]; // pango 12000/65535
const SPAWN_BG: [f32; 4] = [0., 0., 0., 1.];
const TEXT_COLOR: [f32; 4] = [1., 1., 1., 1.];

/// One run of same-styled text in an action label. The old cairo path carried this as pango markup
/// in a `String`; we now build it directly so no markup parser (cairo/pango) is needed. Custom
/// user `hotkey-overlay-title`s render as a single plain span (any markup they contain is
/// stripped).
struct LabelSpan {
    text: String,
    mono: bool,
    /// Inline background patch (e.g. the black box behind a spawn command).
    bg: Option<[f32; 4]>,
}

impl LabelSpan {
    fn plain(text: String) -> Self {
        Self {
            text,
            mono: false,
            bg: None,
        }
    }
}

type Label = Vec<LabelSpan>;

pub struct HotkeyOverlay {
    is_open: bool,
    config: Rc<RefCell<Config>>,
    mod_key: ModKey,
    /// The GSettings keybindings, which is where most bindings now live — the
    /// config binds are only what a user has overridden on top.
    ///
    /// A snapshot rather than a borrow, because the bake is cached: a change has
    /// to bump [`revision`](Self::revision) to be seen, which is what
    /// [`set_keybindings`](Self::set_keybindings) is for.
    keybindings: Vec<GnomeKeybinding>,
    /// Content-sized bake, keyed by `(scale, revision)`. The content depends only on the config,
    /// the keybindings and the mod key, so [`revision`](Self::revision) is a generation counter
    /// bumped whenever those change (a scale change is already a fresh key).
    cache: RefCell<ContentCache>,
    revision: u64,
}

impl HotkeyOverlay {
    pub fn new(
        config: Rc<RefCell<Config>>,
        mod_key: ModKey,
        keybindings: &[GnomeKeybinding],
    ) -> Self {
        Self {
            is_open: false,
            config,
            mod_key,
            keybindings: keybindings.to_vec(),
            cache: RefCell::new(ContentCache::new()),
            revision: 0,
        }
    }

    /// Take a fresh copy of the keybinding model, re-baking if it actually changed.
    pub fn set_keybindings(&mut self, keybindings: &[GnomeKeybinding]) {
        if self.keybindings != keybindings {
            self.keybindings = keybindings.to_vec();
            self.revision = self.revision.wrapping_add(1);
        }
    }

    pub fn show(&mut self) -> bool {
        if !self.is_open {
            self.is_open = true;
            true
        } else {
            false
        }
    }

    pub fn hide(&mut self) -> bool {
        if self.is_open {
            self.is_open = false;
            true
        } else {
            false
        }
    }

    pub fn is_open(&self) -> bool {
        self.is_open
    }

    pub fn on_hotkey_config_updated(&mut self, mod_key: ModKey) {
        self.mod_key = mod_key;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
    ) -> Option<TextureRenderElement<VkTexture>> {
        if !self.is_open {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);
        let config = self.config.borrow();
        let mod_key = self.mod_key;

        // FIXME: should probably use the working area rather than view size.
        let texture = {
            let mut cache = self.cache.borrow_mut();
            match widget::bake_content(
                renderer,
                &mut cache,
                scale,
                self.revision,
                |r| prepare(r, &config, &self.keybindings, mod_key, scale),
                |frame, phys, layout| paint(frame, phys, layout, scale),
            ) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    // Empty table (nothing bound with `hide-not-bound`) or a GPU error: draw
                    // nothing. Not cached, so it re-attempts next frame — cheap while empty.
                    debug!("not rendering the hotkey overlay: {err:#}");
                    None
                }
            }
        }?;

        let size = Size::<f64, _>::from((
            f64::from(texture.width()) / scale,
            f64::from(texture.height()) / scale,
        ));
        let location = (output_size.to_f64().to_point() - size.to_point()).downscale(2.);
        let mut location = location.to_physical_precise_round(scale).to_logical(scale);
        location.x = f64::max(0., location.x);
        location.y = f64::max(0., location.y);

        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            0.9,
            None,
            None,
            Kind::Unspecified,
        );

        Some(elem)
    }

    pub fn a11y_text(&self) -> String {
        let config = self.config.borrow();
        let actions = collect_actions(&config, &self.keybindings);

        let mut buf = String::new();
        writeln!(&mut buf, "{TITLE}").unwrap();

        for action in actions {
            let (key, label) = format_bind(&self.keybindings, &action);

            let key = key.map(|key| key_name(true, self.mod_key, &key));
            let key = key.as_deref().unwrap_or("not bound");

            let action: String = label.iter().map(|s| s.text.as_str()).collect();

            writeln!(&mut buf, "{key} {action}").unwrap();
        }

        buf
    }
}

/// The key shown for `action`, and its label. `None` for the key means the action is not
/// bound to anything showable.
fn format_bind(keybindings: &[GnomeKeybinding], action: &Action) -> (Option<Key>, Label) {
    (keybinding_key(keybindings, action), action_label(action))
}

/// The first showable accelerator bound to `action` in the settings model.
///
/// "Showable" drops keycode accelerators: those name a physical key with no
/// layout-independent name to print. An action bound only that way reads as unbound, which
/// is the honest answer for an overlay that prints key names.
fn keybinding_key(keybindings: &[GnomeKeybinding], action: &Action) -> Option<Key> {
    keybindings
        .iter()
        .filter(|kb| action_for_keybinding(&kb.action).as_ref() == Some(action))
        .find_map(|kb| kb.accels.iter().find_map(key_for_accel))
}

/// Styled label for a built-in action. Only spawn actions carry structure (a monospace command on a
/// black patch); everything else is a single plain span.
fn action_label(action: &Action) -> Label {
    match action {
        Action::Spawn(args) => spawn_label(args.first().map(String::as_str).unwrap_or("")),
        Action::SpawnSh(command) => {
            spawn_label(command.split_ascii_whitespace().next().unwrap_or(""))
        }
        _ => vec![LabelSpan::plain(action_name(action))],
    }
}

fn spawn_label(command: &str) -> Label {
    vec![
        LabelSpan::plain("Spawn ".to_string()),
        LabelSpan {
            text: command.to_string(),
            mono: true,
            bg: Some(SPAWN_BG),
        },
    ]
}

fn bound_actions(keybindings: &[GnomeKeybinding]) -> Vec<Action> {
    keybindings
        .iter()
        .filter(|kb| !kb.accels.is_empty())
        .filter_map(|kb| action_for_keybinding(&kb.action))
        .collect()
}

fn collect_actions(config: &Config, keybindings: &[GnomeKeybinding]) -> Vec<Action> {
    let bound = bound_actions(keybindings);

    // Collect actions that we want to show.
    let mut actions = vec![Action::ShowHotkeyOverlay];

    // Prefer Quit(false) if found, otherwise try Quit(true), and if there's neither, fall back to
    // Quit(false).
    if bound.contains(&Action::Quit(false)) {
        actions.push(Action::Quit(false));
    } else if bound.contains(&Action::Quit(true)) {
        actions.push(Action::Quit(true));
    } else {
        actions.push(Action::Quit(false));
    }

    actions.extend([
        Action::CloseWindow,
        Action::FocusColumnLeft,
        Action::FocusColumnRight,
        Action::MoveColumnLeft,
        Action::MoveColumnRight,
        Action::FocusWorkspaceDown,
        Action::FocusWorkspaceUp,
    ]);

    // Prefer move-column-to-workspace-down, but fall back to move-window-to-workspace-down.
    if let Some(action) = bound
        .iter()
        .find(|action| matches!(action, Action::MoveColumnToWorkspaceDown(_)))
    {
        actions.push(action.clone());
    } else if bound
        .iter()
        .any(|action| matches!(action, Action::MoveWindowToWorkspaceDown(_)))
    {
        actions.push(Action::MoveWindowToWorkspaceDown(true));
    } else {
        actions.push(Action::MoveColumnToWorkspaceDown(true));
    }

    // Same for -up.
    if let Some(action) = bound
        .iter()
        .find(|action| matches!(action, Action::MoveColumnToWorkspaceUp(_)))
    {
        actions.push(action.clone());
    } else if bound
        .iter()
        .any(|action| matches!(action, Action::MoveWindowToWorkspaceUp(_)))
    {
        actions.push(Action::MoveWindowToWorkspaceUp(true));
    } else {
        actions.push(Action::MoveColumnToWorkspaceUp(true));
    }

    actions.extend([
        Action::SwitchPresetColumnWidth,
        Action::MaximizeColumn,
        Action::ConsumeOrExpelWindowLeft,
        Action::ConsumeOrExpelWindowRight,
        Action::ToggleWindowFloating,
        Action::SwitchFocusBetweenFloatingAndTiling,
        Action::ToggleOverview,
    ]);

    // Screenshot is not as important, can omit if not bound.
    if let Some(action) = bound
        .iter()
        .find(|action| matches!(action, Action::Screenshot(_, _)))
    {
        actions.push(action.clone());
    }

    if config.hotkey_overlay.hide_not_bound {
        // Only keep actions that have been bound
        actions.retain(|action| bound.contains(action));
    }

    actions
}

/// A single laid-out table row: the shaped key + action runs and their draw origins, plus the
/// pre-clipped background patches (the grey key patch, any black spawn-command patch).
struct RowLayout {
    key_run: ShapedParagraph,
    key_origin: Point<i32, Physical>,
    key_patch: Option<Rectangle<i32, Physical>>,
    action_run: ShapedParagraph,
    action_origin: Point<i32, Physical>,
    /// Inline action patches (rect + color), already clipped to the inner panel.
    action_patches: Vec<(Rectangle<i32, Physical>, [f32; 4])>,
}

/// The computed physical layout of the whole overlay panel, produced by [`prepare`] and drawn by
/// [`paint`].
struct OverlayLayout {
    title_run: ShapedParagraph,
    title_origin: Point<i32, Physical>,
    inner: Rectangle<i32, Physical>,
    rows: Vec<RowLayout>,
}

/// Shape the whole hotkey table and compute its content-sized layout — the prepare phase for
/// [`widget::bake_content`]. A dark panel with a light-blue border, a centered bold title, then one
/// row per action: a monospace key on a grey patch (a black patch behind a spawn command) and the
/// action label. No cairo/pango.
fn prepare(
    renderer: &mut VulkanRenderer,
    config: &Config,
    keybindings: &[GnomeKeybinding],
    mod_key: ModKey,
    scale: f64,
) -> anyhow::Result<(Size<i32, Physical>, OverlayLayout)> {
    let _span = tracy_client::span!("hotkey_overlay::prepare");

    let font_px = font_px() * scale;
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let line_interval: i32 = to_physical_precise_round(scale, LINE_INTERVAL);
    // Keep the border width even to avoid blurry edges.
    let border: i32 = ((f64::from(BORDER) / 2. * scale).round() as i32).max(1);
    // Horizontal / vertical breathing room around the grey/black inline patches.
    let hpad: i32 = (font_px * 0.3).round() as i32;
    let vpad: i32 = (font_px * 0.12).round() as i32;

    let rows: Vec<(String, Label)> = collect_actions(config, keybindings)
        .into_iter()
        .map(|action| format_bind(keybindings, &action))
        .map(|(key, label)| {
            let key = key.map(|key| key_name(false, mod_key, &key));
            let key = key.as_deref().unwrap_or("(not bound)").to_string();
            (key, label)
        })
        .collect();
    anyhow::ensure!(!rows.is_empty(), "no hotkeys to show");

    // Shape everything up front (each run owns its atlas), then measure, then place. A generous
    // non-wrapping width (logical px) — every line is short.
    const WRAP: f64 = 100_000.;
    let mut shaper = TextShaper::new(renderer, scale);
    let title_run =
        shaper.paragraph(&[ParagraphSpan::new(TITLE, FONT_PT).bold()], WRAP, FONT_PT)?;
    let key_runs = rows
        .iter()
        .map(|(key, _)| shaper.paragraph(&[ParagraphSpan::new(key, FONT_PT).mono()], WRAP, FONT_PT))
        .collect::<Result<Vec<_>, _>>()?;
    let action_runs = rows
        .iter()
        .map(|(_, label)| {
            let spans: Vec<ParagraphSpan> = label
                .iter()
                .map(|s| {
                    let span = ParagraphSpan::new(&s.text, FONT_PT);
                    if s.mono {
                        span.mono()
                    } else {
                        span
                    }
                })
                .collect();
            shaper.paragraph(&spans, WRAP, FONT_PT)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let (tix, tiy, tiw, tih) = title_run.ink_bounds();
    let key_ink: Vec<(i32, i32, i32, i32)> = key_runs.iter().map(|r| r.ink_bounds()).collect();
    let act_ink: Vec<(i32, i32, i32, i32)> = action_runs.iter().map(|r| r.ink_bounds()).collect();

    let key_width = key_ink.iter().map(|b| b.2).max().unwrap_or(0);
    let action_width = act_ink.iter().map(|b| b.2).max().unwrap_or(0);
    // Uniform row advance from the deepest ink (line-box top to lowest descender) across all rows.
    let line_bottom = key_ink
        .iter()
        .chain(&act_ink)
        .map(|b| b.1 + b.3)
        .max()
        .unwrap_or(0);
    let row_advance = line_bottom + line_interval;

    let n = rows.len() as i32;
    let table_top = padding + tih + padding;
    let width = key_width + action_width + padding * 3;
    let height = table_top + (n - 1) * row_advance + line_bottom + padding;

    let size = Size::<i32, Physical>::from((width, height));
    let inner = Rectangle::new(
        Point::from((border, border)),
        Size::from(((width - border * 2).max(0), (height - border * 2).max(0))),
    );

    let title_origin = Point::<i32, Physical>::from(((width - tiw) / 2 - tix, padding - tiy));

    let action_x = padding + key_width + padding;
    let mut row_layouts = Vec::with_capacity(rows.len());
    for (i, (_, label)) in rows.iter().enumerate() {
        let y_line = table_top + i as i32 * row_advance;

        // Key cell: grey patch hugging the mono text.
        let (kx, ky, kw, kh) = key_ink[i];
        let key_origin = Point::<i32, Physical>::from((padding - kx, y_line));
        let key_patch = (kw > 0 && kh > 0)
            .then(|| {
                Rectangle::new(
                    Point::from((key_origin.x + kx - hpad, key_origin.y + ky - vpad)),
                    Size::from((kw + hpad * 2, kh + vpad * 2)),
                )
                .intersection(inner)
            })
            .flatten();

        // Action cell: any inline background patches (spawn command).
        let (ax, ..) = act_ink[i];
        let act_origin = Point::<i32, Physical>::from((action_x - ax, y_line));
        let mut action_patches = Vec::new();
        for (si, span) in label.iter().enumerate() {
            let Some(bg) = span.bg else { continue };
            let (sx, sy, sw, sh) = action_runs[i].span_ink_bounds(si as u32);
            if sw > 0 && sh > 0 {
                let patch = Rectangle::new(
                    Point::from((act_origin.x + sx - hpad / 2, act_origin.y + sy - vpad)),
                    Size::from((sw + hpad, sh + vpad * 2)),
                );
                if let Some(patch) = patch.intersection(inner) {
                    action_patches.push((patch, bg));
                }
            }
        }

        row_layouts.push(RowLayout {
            key_run: key_runs[i].clone(),
            key_origin,
            key_patch,
            action_run: action_runs[i].clone(),
            action_origin: act_origin,
            action_patches,
        });
    }

    Ok((
        size,
        OverlayLayout {
            title_run,
            title_origin,
            inner,
            rows: row_layouts,
        },
    ))
}

/// Draw the bordered panel, the title, and every row — the paint phase for
/// [`widget::bake_content`].
fn paint(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    layout: &OverlayLayout,
    scale: f64,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);

    // Light-blue border = whole panel border-coloured, then the inner rect dark.
    p.clear(BORDER_COLOR)?;
    p.fill_rect_px(layout.inner, PANEL_BG)?;

    p.paragraph(&layout.title_run, layout.title_origin, TEXT_COLOR)?;

    for row in &layout.rows {
        if let Some(patch) = row.key_patch {
            p.fill_rect_px(patch, KEY_BG)?;
        }
        p.paragraph(&row.key_run, row.key_origin, TEXT_COLOR)?;

        for (patch, bg) in &row.action_patches {
            p.fill_rect_px(*patch, *bg)?;
        }
        p.paragraph(&row.action_run, row.action_origin, TEXT_COLOR)?;
    }

    Ok(())
}

fn action_name(action: &Action) -> String {
    match action {
        Action::Quit(_) => String::from("Exit niri"),
        Action::ShowHotkeyOverlay => String::from("Show Important Hotkeys"),
        Action::CloseWindow => String::from("Close Focused Window"),
        Action::FocusColumnLeft => String::from("Focus Column to the Left"),
        Action::FocusColumnRight => String::from("Focus Column to the Right"),
        Action::MoveColumnLeft => String::from("Move Column Left"),
        Action::MoveColumnRight => String::from("Move Column Right"),
        Action::FocusWorkspaceDown => String::from("Switch Workspace Down"),
        Action::FocusWorkspaceUp => String::from("Switch Workspace Up"),
        Action::MoveColumnToWorkspaceDown(_) => String::from("Move Column to Workspace Down"),
        Action::MoveColumnToWorkspaceUp(_) => String::from("Move Column to Workspace Up"),
        Action::MoveWindowToWorkspaceDown(_) => String::from("Move Window to Workspace Down"),
        Action::MoveWindowToWorkspaceUp(_) => String::from("Move Window to Workspace Up"),
        Action::SwitchPresetColumnWidth => String::from("Switch Preset Column Widths"),
        Action::MaximizeColumn => String::from("Maximize Column"),
        Action::ConsumeOrExpelWindowLeft => String::from("Consume or Expel Window Left"),
        Action::ConsumeOrExpelWindowRight => String::from("Consume or Expel Window Right"),
        Action::ToggleWindowFloating => String::from("Move Window Between Floating and Tiling"),
        Action::SwitchFocusBetweenFloatingAndTiling => {
            String::from("Switch Focus Between Floating and Tiling")
        }
        Action::ToggleOverview => String::from("Open the Overview"),
        Action::Screenshot(_, _) => String::from("Take a Screenshot"),
        // Spawn actions are handled structurally in `action_label`; this is only a plain fallback.
        Action::Spawn(args) => {
            format!("Spawn {}", args.first().map(String::as_str).unwrap_or(""))
        }
        Action::SpawnSh(command) => format!(
            "Spawn {}",
            command.split_ascii_whitespace().next().unwrap_or("")
        ),
        _ => String::from("FIXME: Unknown"),
    }
}

fn key_name(screen_reader: bool, mod_key: ModKey, key: &Key) -> String {
    let mut name = String::new();

    let has_comp_mod = key.modifiers.contains(Modifiers::COMPOSITOR);

    // Compositor mod goes first.
    if has_comp_mod {
        match mod_key {
            ModKey::Super => {
                name.push_str("Super + ");
            }
            ModKey::Alt => {
                name.push_str("Alt + ");
            }
            ModKey::Shift => {
                name.push_str("Shift + ");
            }
            ModKey::Ctrl => {
                name.push_str("Ctrl + ");
            }
            ModKey::IsoLevel3Shift => {
                name.push_str("Mod5 + ");
            }
            ModKey::IsoLevel5Shift => {
                name.push_str("Mod3 + ");
            }
        }
    }

    if key.modifiers.contains(Modifiers::SUPER) && !(has_comp_mod && mod_key == ModKey::Super) {
        name.push_str("Super + ");
    }
    if key.modifiers.contains(Modifiers::CTRL) && !(has_comp_mod && mod_key == ModKey::Ctrl) {
        name.push_str("Ctrl + ");
    }
    if key.modifiers.contains(Modifiers::SHIFT) && !(has_comp_mod && mod_key == ModKey::Shift) {
        name.push_str("Shift + ");
    }
    if key.modifiers.contains(Modifiers::ALT) && !(has_comp_mod && mod_key == ModKey::Alt) {
        name.push_str("Alt + ");
    }
    if key.modifiers.contains(Modifiers::ISO_LEVEL3_SHIFT)
        && !(has_comp_mod && mod_key == ModKey::IsoLevel3Shift)
    {
        name.push_str("Mod5 + ");
    }
    if key.modifiers.contains(Modifiers::ISO_LEVEL5_SHIFT)
        && !(has_comp_mod && mod_key == ModKey::IsoLevel5Shift)
    {
        name.push_str("Mod3 + ");
    }

    let pretty = match key.trigger {
        Trigger::Keysym(keysym) => prettify_keysym_name(screen_reader, &keysym_get_name(keysym)),
        Trigger::MouseLeft => String::from("Mouse Left"),
        Trigger::MouseRight => String::from("Mouse Right"),
        Trigger::MouseMiddle => String::from("Mouse Middle"),
        Trigger::MouseBack => String::from("Mouse Back"),
        Trigger::MouseForward => String::from("Mouse Forward"),
        Trigger::WheelScrollDown => String::from("Wheel Scroll Down"),
        Trigger::WheelScrollUp => String::from("Wheel Scroll Up"),
        Trigger::WheelScrollLeft => String::from("Wheel Scroll Left"),
        Trigger::WheelScrollRight => String::from("Wheel Scroll Right"),
        Trigger::TouchpadScrollDown => String::from("Touchpad Scroll Down"),
        Trigger::TouchpadScrollUp => String::from("Touchpad Scroll Up"),
        Trigger::TouchpadScrollLeft => String::from("Touchpad Scroll Left"),
        Trigger::TouchpadScrollRight => String::from("Touchpad Scroll Right"),
        Trigger::TabletStylusButton1 => String::from("Tablet Stylus Button 1"),
        Trigger::TabletStylusButton2 => String::from("Tablet Stylus Button 2"),
        Trigger::TabletStylusButton3 => String::from("Tablet Stylus Button 3"),
    };
    name.push_str(&pretty);

    name
}

fn prettify_keysym_name(screen_reader: bool, name: &str) -> String {
    let name = if screen_reader {
        name
    } else {
        match name {
            "slash" => "/",
            "comma" => ",",
            "period" => ".",
            "minus" => "-",
            "equal" => "=",
            "grave" => "`",
            "bracketleft" => "[",
            "bracketright" => "]",
            "adiaeresis" => "Ä",
            "ediaeresis" => "Ë",
            "idiaeresis" => "Ï",
            "odiaeresis" => "Ö",
            "udiaeresis" => "Ü",
            "ydiaeresis" => "Ÿ",
            "wdiaeresis" => "Ẅ",
            _ => name,
        }
    };

    let name = match name {
        "Next" => "Page Down",
        "Prior" => "Page Up",
        "Print" => "PrtSc",
        "Return" => "Enter",
        "space" => "Space",
        _ => name,
    };

    if name.len() == 1 && name.is_ascii() {
        name.to_ascii_uppercase()
    } else {
        name.into()
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    #[track_caller]
    fn check(keybindings: &[GnomeKeybinding], action: Action) -> String {
        let (key, label) = format_bind(keybindings, &action);
        let key = key.map(|key| key_name(false, ModKey::Super, &key));
        let key = key.as_deref().unwrap_or("(not bound)");
        let title: String = label.iter().map(|s| s.text.as_str()).collect();
        format!(" {key} : {title}")
    }

    /// Bindings live in GSettings and nowhere else, so this is the whole of what the
    /// overlay can show.
    #[test]
    fn keybindings_come_from_the_settings_model() {
        let keybindings = crate::gnome::GnomeSettings::default().keybindings;

        assert_snapshot!(
            check(&keybindings, Action::CloseWindow),
            @" Alt + F4 : Close Focused Window"
        );
        assert_snapshot!(
            check(&keybindings, Action::FocusColumnLeft),
            @" Super + Alt + H : Focus Column to the Left"
        );

        // An action nothing binds still gets a row, marked as such. `toggle-overview` is the
        // real case: GNOME ships it unbound, reachable by the overlay key instead.
        assert_snapshot!(
            check(&keybindings, Action::ToggleOverview),
            @" (not bound) : Open the Overview"
        );
        assert_snapshot!(check(&[], Action::CloseWindow), @" (not bound) : Close Focused Window");
    }

    /// `hide-not-bound` drops actions nothing binds. With the settings model as the only
    /// source, that must not empty the overlay out.
    #[test]
    fn hide_not_bound_still_sees_the_settings_model() {
        let keybindings = crate::gnome::GnomeSettings::default().keybindings;
        let config = Config::parse_mem("hotkey-overlay { hide-not-bound; }").unwrap();

        let actions = collect_actions(&config, &keybindings);
        assert!(actions.contains(&Action::CloseWindow));
        assert!(actions.contains(&Action::FocusColumnLeft));

        // And the empty model is what actually hides them.
        assert!(collect_actions(&config, &[]).is_empty());
    }
}
