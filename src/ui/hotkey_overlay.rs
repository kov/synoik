use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use niri_config::{Action, Bind, Config, Key, ModKey, Modifiers, Trigger};
use niri_vk::text::{SpanFamily, TextSpan};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind as _, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::input::keyboard::xkb::keysym_get_name;
use smithay::output::{Output, WeakOutput};
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size, Transform};

use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

const PADDING: i32 = 8;
const FONT_PX: f64 = crate::ui::pt_to_px(11.);
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
    buffers: RefCell<HashMap<WeakOutput, RenderedOverlay>>,
}

pub struct RenderedOverlay {
    texture: Option<VkTexture>,
    scale: f64,
    context: Option<ContextId<VkTexture>>,
}

impl HotkeyOverlay {
    pub fn new(config: Rc<RefCell<Config>>, mod_key: ModKey) -> Self {
        Self {
            is_open: false,
            config,
            mod_key,
            buffers: RefCell::new(HashMap::new()),
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
        self.buffers.borrow_mut().clear();
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

        let mut buffers = self.buffers.borrow_mut();
        buffers.retain(|output, _| output.is_alive());

        let context = renderer.context_id();

        // FIXME: should probably use the working area rather than view size.
        let weak = output.downgrade();
        if let Some(rendered) = buffers.get(&weak) {
            if rendered.scale != scale || rendered.context.as_ref() != Some(&context) {
                buffers.remove(&weak);
            }
        }

        let rendered = buffers.entry(weak).or_insert_with(|| {
            // The overlay is drawn straight into a VkTexture by the owned renderer.
            let texture = generate(renderer, &self.config.borrow(), self.mod_key, scale).ok();
            RenderedOverlay {
                texture,
                scale,
                context: Some(context),
            }
        });
        let texture = rendered.texture.as_ref()?;

        let size = Size::<f64, _>::from((
            f64::from(texture.width()) / scale,
            f64::from(texture.height()) / scale,
        ));
        let location = (output_size.to_f64().to_point() - size.to_point()).downscale(2.);
        let mut location = location.to_physical_precise_round(scale).to_logical(scale);
        location.x = f64::max(0., location.x);
        location.y = f64::max(0., location.y);

        let buffer = TextureBuffer::from_texture(
            renderer,
            texture.clone(),
            scale,
            Transform::Normal,
            Vec::new(),
        );

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
        let actions = collect_actions(&config);

        let mut buf = String::new();
        writeln!(&mut buf, "{TITLE}").unwrap();

        for action in actions {
            let Some((key, label)) = format_bind(&config.binds.0, action) else {
                continue;
            };

            let key = key.map(|key| key_name(true, self.mod_key, &key));
            let key = key.as_deref().unwrap_or("not bound");

            let action: String = label.iter().map(|s| s.text.as_str()).collect();

            writeln!(&mut buf, "{key} {action}").unwrap();
        }

        buf
    }
}

fn format_bind(binds: &[Bind], action: &Action) -> Option<(Option<Key>, Label)> {
    let mut bind_with_non_null = None;
    let mut bind_with_custom_title = None;
    let mut found_null_title = false;

    for bind in binds {
        if bind.action != *action {
            continue;
        }

        match &bind.hotkey_overlay_title {
            Some(Some(_)) => {
                bind_with_custom_title.get_or_insert(bind);
            }
            Some(None) => {
                found_null_title = true;
            }
            None => {
                bind_with_non_null.get_or_insert(bind);
            }
        }
    }

    if bind_with_custom_title.is_none() && found_null_title {
        return None;
    }

    let mut custom_title = None;
    let key = if let Some(bind) = bind_with_custom_title.or(bind_with_non_null) {
        if let Some(Some(custom)) = &bind.hotkey_overlay_title {
            custom_title = Some(custom.clone());
        }

        Some(bind.key)
    } else {
        None
    };
    // A custom title is user text: render it plain (any pango markup it carries is stripped).
    let label = match custom_title {
        Some(title) => vec![LabelSpan::plain(strip_markup(&title))],
        None => action_label(action),
    };

    Some((key, label))
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

/// Drop pango-markup tags and unescape the basic entities, so a user-supplied title renders as
/// clean plain text without a markup parser.
fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn collect_actions(config: &Config) -> Vec<&Action> {
    let binds = &config.binds.0;

    // Collect actions that we want to show.
    let mut actions = vec![&Action::ShowHotkeyOverlay];

    // Prefer Quit(false) if found, otherwise try Quit(true), and if there's neither, fall back to
    // Quit(false).
    if binds.iter().any(|bind| bind.action == Action::Quit(false)) {
        actions.push(&Action::Quit(false));
    } else if binds.iter().any(|bind| bind.action == Action::Quit(true)) {
        actions.push(&Action::Quit(true));
    } else {
        actions.push(&Action::Quit(false));
    }

    actions.extend(&[
        &Action::CloseWindow,
        &Action::FocusColumnLeft,
        &Action::FocusColumnRight,
        &Action::MoveColumnLeft,
        &Action::MoveColumnRight,
        &Action::FocusWorkspaceDown,
        &Action::FocusWorkspaceUp,
    ]);

    // Prefer move-column-to-workspace-down, but fall back to move-window-to-workspace-down.
    if let Some(bind) = binds
        .iter()
        .find(|bind| matches!(bind.action, Action::MoveColumnToWorkspaceDown(_)))
    {
        actions.push(&bind.action);
    } else if binds
        .iter()
        .any(|bind| matches!(bind.action, Action::MoveWindowToWorkspaceDown(_)))
    {
        actions.push(&Action::MoveWindowToWorkspaceDown(true));
    } else {
        actions.push(&Action::MoveColumnToWorkspaceDown(true));
    }

    // Same for -up.
    if let Some(bind) = binds
        .iter()
        .find(|bind| matches!(bind.action, Action::MoveColumnToWorkspaceUp(_)))
    {
        actions.push(&bind.action);
    } else if binds
        .iter()
        .any(|bind| matches!(bind.action, Action::MoveWindowToWorkspaceUp(_)))
    {
        actions.push(&Action::MoveWindowToWorkspaceUp(true));
    } else {
        actions.push(&Action::MoveColumnToWorkspaceUp(true));
    }

    actions.extend(&[
        &Action::SwitchPresetColumnWidth,
        &Action::MaximizeColumn,
        &Action::ConsumeOrExpelWindowLeft,
        &Action::ConsumeOrExpelWindowRight,
        &Action::ToggleWindowFloating,
        &Action::SwitchFocusBetweenFloatingAndTiling,
        &Action::ToggleOverview,
    ]);

    // Screenshot is not as important, can omit if not bound.
    if let Some(bind) = binds
        .iter()
        .find(|bind| matches!(bind.action, Action::Screenshot(_, _)))
    {
        actions.push(&bind.action);
    }

    // Add actions with a custom hotkey-overlay-title.
    for bind in binds {
        if matches!(bind.hotkey_overlay_title, Some(Some(_))) {
            // Avoid duplicate actions.
            if !actions.contains(&&bind.action) {
                actions.push(&bind.action);
            }
        }
    }

    // Add the spawn actions.
    for bind in binds.iter().filter(|bind| {
        matches!(bind.action, Action::Spawn(_) | Action::SpawnSh(_))
            // Only show binds with Mod or Super to filter out stuff like volume up/down.
            && (bind.key.modifiers.contains(Modifiers::COMPOSITOR)
                || bind.key.modifiers.contains(Modifiers::SUPER))
            // Also filter out wheel and touchpad scroll binds.
            && matches!(bind.key.trigger, Trigger::Keysym(_))
    }) {
        let action = &bind.action;

        // We only show one bind for each action, so we need to deduplicate the Spawn actions.
        if !actions.contains(&action) {
            actions.push(action);
        }
    }

    if config.hotkey_overlay.hide_not_bound {
        // Only keep actions that have been bound
        actions.retain(|&action| binds.iter().any(|bind| bind.action == *action))
    }

    actions
}

/// Draw the whole hotkey table straight into a `VkTexture`: a dark panel with a light-blue border,
/// a centered bold title, then one row per action — a monospace key on a grey patch (a black patch
/// behind a spawn command) and the action label. No cairo/pango.
fn generate(
    renderer: &mut VulkanRenderer,
    config: &Config,
    mod_key: ModKey,
    scale: f64,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("hotkey_overlay::generate");

    let px = (FONT_PX * scale) as f32;
    let padding: i32 = to_physical_precise_round(scale, PADDING);
    let line_interval: i32 = to_physical_precise_round(scale, LINE_INTERVAL);
    // Keep the border width even to avoid blurry edges.
    let border: i32 = ((f64::from(BORDER) / 2. * scale).round() as i32).max(1);
    // Horizontal / vertical breathing room around the grey/black inline patches.
    let hpad: i32 = (px * 0.3).round() as i32;
    let vpad: i32 = (px * 0.12).round() as i32;

    let rows: Vec<(String, Label)> = collect_actions(config)
        .into_iter()
        .filter_map(|action| format_bind(&config.binds.0, action))
        .map(|(key, label)| {
            let key = key.map(|key| key_name(false, mod_key, &key));
            let key = key.as_deref().unwrap_or("(not bound)").to_string();
            (key, label)
        })
        .collect();
    anyhow::ensure!(!rows.is_empty(), "no hotkeys to show");

    // Shape everything up front (each run owns its atlas), then measure, then draw.
    const WRAP: f32 = 100_000.;
    let title_run = renderer.build_glyph_paragraph(
        &[TextSpan {
            text: TITLE,
            family: SpanFamily::Sans,
            bold: true,
            px,
        }],
        WRAP,
        px,
    )?;
    let key_runs = rows
        .iter()
        .map(|(key, _)| {
            renderer.build_glyph_paragraph(
                &[TextSpan {
                    text: key,
                    family: SpanFamily::Mono,
                    bold: false,
                    px,
                }],
                WRAP,
                px,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let action_runs = rows
        .iter()
        .map(|(_, label)| {
            let spans: Vec<TextSpan> = label
                .iter()
                .map(|s| TextSpan {
                    text: &s.text,
                    family: if s.mono {
                        SpanFamily::Mono
                    } else {
                        SpanFamily::Sans
                    },
                    bold: false,
                    px,
                })
                .collect();
            renderer.build_glyph_paragraph(&spans, WRAP, px)
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
    let full = Rectangle::from_size(size);
    let inner = Rectangle::new(
        Point::from((border, border)),
        Size::from(((width - border * 2).max(0), (height - border * 2).max(0))),
    );

    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((width, height)),
    )?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;

        // Light-blue border = whole panel border-coloured, then the inner rect dark.
        frame.clear(Color32F::from(BORDER_COLOR), &[full])?;
        frame.clear(Color32F::from(PANEL_BG), &[inner])?;

        // Centered title.
        let title_origin = Point::<i32, Physical>::from(((width - tiw) / 2 - tix, padding - tiy));
        frame.render_glyphs(&title_run, title_origin, TEXT_COLOR, full, &[full])?;

        for i in 0..rows.len() {
            let y_line = table_top + i as i32 * row_advance;

            // Key cell: grey patch hugging the mono text, then the text.
            let (kx, ky, kw, kh) = key_ink[i];
            let key_origin = Point::<i32, Physical>::from((padding - kx, y_line));
            if kw > 0 && kh > 0 {
                let patch = Rectangle::new(
                    Point::from((key_origin.x + kx - hpad, key_origin.y + ky - vpad)),
                    Size::from((kw + hpad * 2, kh + vpad * 2)),
                );
                if let Some(patch) = patch.intersection(inner) {
                    frame.clear(Color32F::from(KEY_BG), &[patch])?;
                }
            }
            frame.render_glyphs(&key_runs[i], key_origin, TEXT_COLOR, full, &[full])?;

            // Action cell: any inline background patches (spawn command), then the text.
            let (ax, ..) = act_ink[i];
            let action_x = padding + key_width + padding;
            let act_origin = Point::<i32, Physical>::from((action_x - ax, y_line));
            for (si, span) in rows[i].1.iter().enumerate() {
                let Some(bg) = span.bg else { continue };
                let (sx, sy, sw, sh) = action_runs[i].span_ink_bounds(si as u32);
                if sw > 0 && sh > 0 {
                    let patch = Rectangle::new(
                        Point::from((act_origin.x + sx - hpad / 2, act_origin.y + sy - vpad)),
                        Size::from((sw + hpad, sh + vpad * 2)),
                    );
                    if let Some(patch) = patch.intersection(inner) {
                        frame.clear(Color32F::from(bg), &[patch])?;
                    }
                }
            }
            frame.render_glyphs(&action_runs[i], act_origin, TEXT_COLOR, full, &[full])?;
        }

        let _sync = frame.finish()?;
    }
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
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
    fn check(config: &str, action: Action) -> String {
        let config = Config::parse_mem(config).unwrap();
        if let Some((key, label)) = format_bind(&config.binds.0, &action) {
            let key = key.map(|key| key_name(false, ModKey::Super, &key));
            let key = key.as_deref().unwrap_or("(not bound)");
            let title: String = label.iter().map(|s| s.text.as_str()).collect();
            format!(" {key} : {title}")
        } else {
            String::from("None")
        }
    }

    #[test]
    fn test_format_bind() {
        // Not bound.
        assert_snapshot!(check("", Action::Screenshot(true, None)), @" (not bound) : Take a Screenshot");

        // Bound with a default title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Take a Screenshot"
        );

        // Custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="Hello" { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Hello"
        );

        // Prefer first bind.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Take a Screenshot"
        );

        // Prefer bind with custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print hotkey-overlay-title="My Cool Bind" { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" PrtSc : My Cool Bind"
        );

        // Prefer first bind with custom title.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="First" { screenshot; }
                    Print hotkey-overlay-title="My Cool Bind" { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : First"
        );

        // Any bind with null title hides it.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P { screenshot; }
                    Print hotkey-overlay-title=null { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @"None"
        );

        // Custom title takes preference over null.
        assert_snapshot!(
            check(
                r#"binds {
                    Mod+P hotkey-overlay-title="Hello" { screenshot; }
                    Print hotkey-overlay-title=null { screenshot; }
                }"#,
                Action::Screenshot(true, None),
            ),
            @" Super + P : Hello"
        );
    }
}
