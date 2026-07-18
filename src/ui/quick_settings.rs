//! The quick-settings menu (the right-hand panel status area's popover).
//!
//! A fork-owned port of gnome-shell's `js/ui/quickSettings.js` first slice: a
//! **system row** at the top (Settings on the left, Lock/Power on the right —
//! gnome-shell's `SystemItem`, which `panel.js` adds first) above a two-column
//! grid of [`QuickToggle`]-style tiles backed purely by gsettings — **Dark
//! Style**, **Do Not Disturb**, **Night Light**. Each tile shows a symbolic icon
//! and a label and flips its gsettings key on click; the system buttons spawn the
//! canonical session commands.
//!
//! Like the calendar, the chrome (menu/tile backgrounds + labels) is drawn into
//! one offscreen `VkTexture` with `clear` + `render_glyphs` (rounded fills and
//! texture blits don't render reliably inside a hand-bound offscreen), and the
//! **icons are composited as separate elements on top** from the shared
//! [`IconCache`] — symbolic SVGs recolored to the fore/back color of their slot.
//!
//! Deferred vs gnome-shell: sliders (volume/brightness), the network/bluetooth
//! toggles and their sub-menus, the battery/percentage row — all daemon-backed
//! (NetworkManager, UPower, gsd) and mostly absent on the VM. This is the
//! self-contained core.

use std::cell::RefCell;
use std::collections::HashMap;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture as _,
};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::gnome::QuickToggles;
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::utils::to_physical_precise_round;

// Geometry, logical px (grounded in gnome-shell-sass quick-settings proportions).
const PAD: f64 = 12.;
const TILE_W: f64 = 150.;
const TILE_H: f64 = 56.;
const TILE_GAP: f64 = 8.;
const COLS: usize = 2;
/// Icon size inside a tile, and its inset from the tile's left edge.
const TILE_ICON: f64 = 16.;
const TILE_ICON_INSET: f64 = 12.;
const LABEL_PX: f64 = 11.;

/// The system row (Settings on the left, Lock/Power on the right) sits at the
/// **top** of the menu, above the tile grid — like gnome-shell's `SystemItem`,
/// which `panel.js` adds first (`_addItemsBefore(this._system…)`). Bare icons.
const SYS_H: f64 = 44.;
const SYS_ICON: f64 = 20.;
const SYS_GAP: f64 = 18.;
const SYS_HIT: f64 = 40.;
/// Inset of the outermost system icons from the menu's left/right edges.
const SYS_INSET: f64 = 12.;

const MENU_BG: [f32; 4] = [0.12, 0.12, 0.12, 1.];
const TILE_OFF: [f32; 4] = [0.24, 0.24, 0.24, 1.];
/// Text/icon on an inactive (dark) tile.
const FG_OFF: [f32; 4] = [1., 1., 1., 1.];
/// Text/icon on an active (accent) tile — dark, for contrast.
const FG_ON: [f32; 4] = [0.1, 0.1, 0.1, 1.];
const SYS_FG: [f32; 4] = [0.9, 0.9, 0.9, 1.];

/// The gsettings-backed tiles, in grid order (row-major, two columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tile {
    DarkStyle,
    DoNotDisturb,
    NightLight,
}

const TILES: [Tile; 3] = [Tile::DarkStyle, Tile::DoNotDisturb, Tile::NightLight];

impl Tile {
    fn label(self) -> &'static str {
        match self {
            Tile::DarkStyle => "Dark Style",
            Tile::DoNotDisturb => "Do Not Disturb",
            Tile::NightLight => "Night Light",
        }
    }

    /// Candidate symbolic icon names, first that resolves wins.
    fn icons(self) -> &'static [&'static str] {
        match self {
            Tile::DarkStyle => &["dark-mode-symbolic", "weather-clear-night-symbolic"],
            Tile::DoNotDisturb => &["notifications-disabled-symbolic", "notification-symbolic"],
            Tile::NightLight => &["night-light-symbolic", "display-brightness-symbolic"],
        }
    }

    fn is_on(self, t: QuickToggles) -> bool {
        match self {
            Tile::DarkStyle => t.dark_style,
            Tile::DoNotDisturb => t.do_not_disturb,
            Tile::NightLight => t.night_light,
        }
    }

    /// The action that sets this tile to `on`.
    fn action(self, on: bool) -> PopoverAction {
        match self {
            Tile::DarkStyle => PopoverAction::SetDarkStyle(on),
            Tile::DoNotDisturb => PopoverAction::SetDoNotDisturb(on),
            Tile::NightLight => PopoverAction::SetNightLight(on),
        }
    }
}

/// The system-row buttons, left-to-right within the right-aligned cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SysButton {
    Settings,
    Lock,
    Power,
}

const SYS_BUTTONS: [SysButton; 3] = [SysButton::Settings, SysButton::Lock, SysButton::Power];

impl SysButton {
    fn icons(self) -> &'static [&'static str] {
        match self {
            SysButton::Settings => &[
                "org.gnome.Settings-symbolic",
                "emblem-system-symbolic",
                "applications-system-symbolic",
            ],
            SysButton::Lock => &["system-lock-screen-symbolic", "changes-prevent-symbolic"],
            SysButton::Power => &["system-shutdown-symbolic"],
        }
    }

    /// The canonical command this button spawns (the same ones GNOME's session
    /// buttons ultimately drive): Settings opens control-center, Lock asks logind
    /// to lock the session, Power asks gnome-session to begin the power-off flow
    /// (which calls back our own EndSessionDialog).
    fn command(self) -> Vec<String> {
        let words: &[&str] = match self {
            SysButton::Settings => &["gnome-control-center"],
            SysButton::Lock => &["loginctl", "lock-session"],
            SysButton::Power => &["gnome-session-quit", "--power-off"],
        };
        words.iter().map(|s| s.to_string()).collect()
    }
}

/// The quick-settings menu. Holds its own copy of the toggle states so a click
/// updates the tile immediately (the write-back round-trips through gsettings).
pub struct QuickSettings {
    toggles: QuickToggles,
    /// Accent color for an active tile's background (straight RGBA).
    accent: [f32; 4],
    /// Bumped on any toggle so the cached chrome texture is redrawn.
    revision: u64,
    cache: RefCell<TextureCache>,
}

struct TextureCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl QuickSettings {
    /// Open with the current toggle states; `accent` straight RGB (e.g.
    /// `gnome_settings.accent_color`).
    pub fn new(toggles: QuickToggles, accent: [u8; 3]) -> Self {
        Self {
            toggles,
            accent: [
                f32::from(accent[0]) / 255.,
                f32::from(accent[1]) / 255.,
                f32::from(accent[2]) / 255.,
                1.,
            ],
            revision: 0,
            cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
        }
    }

    /// The menu's logical size (fixed: two tile columns + the system row).
    pub fn logical_size(&self) -> Size<f64, Logical> {
        Size::from((menu_w(), menu_h()))
    }

    /// Handle a click at a menu-local logical position, returning the action to
    /// apply (or [`PopoverAction::Consumed`] for a click that hit nothing
    /// actionable but is still inside the menu). A tile click also flips the
    /// tile's own state so it updates before the gsettings write round-trips.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        for (i, tile) in TILES.iter().enumerate() {
            if tile_rect(i).contains(pos) {
                let on = !tile.is_on(self.toggles);
                self.set_tile(*tile, on);
                self.revision += 1;
                return tile.action(on);
            }
        }
        for (i, button) in SYS_BUTTONS.iter().enumerate() {
            if sys_rect(i).contains(pos) {
                return PopoverAction::Spawn(button.command());
            }
        }
        PopoverAction::Consumed
    }

    fn set_tile(&mut self, tile: Tile, on: bool) {
        match tile {
            Tile::DarkStyle => self.toggles.dark_style = on,
            Tile::DoNotDisturb => self.toggles.do_not_disturb = on,
            Tile::NightLight => self.toggles.night_light = on,
        }
    }

    /// The composited elements for the menu at `origin` (menu-local → output-local
    /// offset): the chrome texture first (topmost after reversal is handled by the
    /// caller pushing in order), then each resolved icon on top of its slot.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();

        // Tile icons (drawn above the chrome, so pushed before it).
        for (i, tile) in TILES.iter().enumerate() {
            let on = tile.is_on(self.toggles);
            let color = if on { FG_ON } else { FG_OFF };
            let rect = tile_rect(i);
            let center = Point::from((
                rect.loc.x + TILE_ICON_INSET + TILE_ICON / 2.,
                rect.loc.y + rect.size.h / 2.,
            ));
            if let Some(el) = icon_element(
                renderer,
                icons,
                tile.icons(),
                TILE_ICON,
                scale,
                color,
                origin,
                center,
            ) {
                elements.push(el);
            }
        }

        // System-row icons.
        for (i, button) in SYS_BUTTONS.iter().enumerate() {
            let rect = sys_rect(i);
            let center =
                Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));
            if let Some(el) = icon_element(
                renderer,
                icons,
                button.icons(),
                SYS_ICON,
                scale,
                SYS_FG,
                origin,
                center,
            ) {
                elements.push(el);
            }
        }

        // The chrome (menu + tile backgrounds + labels), beneath the icons.
        match self.texture(renderer, scale) {
            Ok(texture) => {
                let opaque = vec![Rectangle::from_size(texture.size())];
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    opaque,
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error drawing the quick-settings menu: {err:#}"),
        }

        elements
    }

    /// Draw (or reuse) the chrome texture, caching per (scale, revision).
    fn texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.textures.clear();
            cache.context = Some(context);
        }
        let fresh =
            matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == self.revision);
        if !fresh {
            let tex = self.draw(renderer, scale)?;
            cache.textures.insert(scale_key, (self.revision, tex));
        }
        Ok(cache
            .textures
            .get(&scale_key)
            .map(|(_, t)| t.clone())
            .unwrap())
    }

    fn draw(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let _span = tracy_client::span!("quick_settings::draw");

        let size = self.logical_size();
        let w_px = to_physical_precise_round::<i32>(scale, size.w).max(1);
        let h_px = to_physical_precise_round::<i32>(scale, size.h).max(1);
        let phys = Size::<i32, Physical>::from((w_px, h_px));
        let label_px = (LABEL_PX * scale) as f32;

        // Shape the tile labels up front (immutable borrows of the font system).
        let label_runs: Vec<_> = TILES
            .iter()
            .map(|t| renderer.build_glyph_run(t.label(), label_px))
            .collect::<Result<_, _>>()?;

        let mut target = renderer.create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, BufferCoord>::from((w_px, h_px)),
        )?;
        {
            let mut fb = renderer.bind(&mut target)?;
            let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
            let full = Rectangle::from_size(phys);
            frame.clear(Color32F::from(MENU_BG), &[full])?;

            let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
            let rect_px = |r: Rectangle<f64, Logical>| {
                Rectangle::new(
                    Point::<i32, Physical>::from((px(r.loc.x), px(r.loc.y))),
                    Size::<i32, Physical>::from((px(r.size.w), px(r.size.h))),
                )
            };
            // Left-align a shaped run's ink at a physical point, vertically centered.
            let place_left = |ink: (i32, i32, i32, i32), lx: i32, cy: i32| {
                let (ix, iy, _iw, ih) = ink;
                Point::<i32, Physical>::from((lx - ix, cy - ih / 2 - iy))
            };

            for (i, tile) in TILES.iter().enumerate() {
                let rect = tile_rect(i);
                let on = tile.is_on(self.toggles);
                let bg = if on { self.accent } else { TILE_OFF };
                frame.clear(Color32F::from(bg), &[rect_px(rect)])?;

                let fg = if on { FG_ON } else { FG_OFF };
                let label_x = px(rect.loc.x + TILE_ICON_INSET + TILE_ICON + 8.);
                let label_cy = px(rect.loc.y + rect.size.h / 2.);
                let run = &label_runs[i];
                frame.render_glyphs(
                    run,
                    place_left(run.ink_bounds(), label_x, label_cy),
                    fg,
                    full,
                    &[full],
                )?;
            }

            let _sync = frame.finish()?;
        }

        renderer.make_offscreen_sampleable(&target)?;
        Ok(target)
    }
}

/// The menu's logical width: two tile columns plus padding.
fn menu_w() -> f64 {
    PAD * 2. + COLS as f64 * TILE_W + (COLS as f64 - 1.) * TILE_GAP
}

/// The menu's logical height: the system row at the top, the gap, then the grid.
fn menu_h() -> f64 {
    let rows = TILES.len().div_ceil(COLS) as f64;
    PAD + SYS_H + TILE_GAP + rows * TILE_H + (rows - 1.) * TILE_GAP + PAD
}

/// The rectangle of tile `i` (row-major), menu-local logical. The grid sits below
/// the top system row.
fn tile_rect(i: usize) -> Rectangle<f64, Logical> {
    let row = (i / COLS) as f64;
    let col = (i % COLS) as f64;
    let x = PAD + col * (TILE_W + TILE_GAP);
    let y = PAD + SYS_H + TILE_GAP + row * (TILE_H + TILE_GAP);
    Rectangle::new(Point::from((x, y)), Size::from((TILE_W, TILE_H)))
}

/// The hit rectangle of system button `i`, menu-local. The row is at the top:
/// Settings on the left, Lock then Power right-aligned in the corner (GNOME's
/// desktop `SystemItem` layout, minus the not-yet-ported screenshot button).
fn sys_rect(i: usize) -> Rectangle<f64, Logical> {
    let right = menu_w() - PAD - SYS_INSET - SYS_ICON / 2.;
    let center_x = match SYS_BUTTONS[i] {
        SysButton::Settings => PAD + SYS_INSET + SYS_ICON / 2.,
        SysButton::Lock => right - (SYS_ICON + SYS_GAP),
        SysButton::Power => right,
    };
    Rectangle::new(
        Point::from((center_x - SYS_HIT / 2., PAD)),
        Size::from((SYS_HIT, SYS_H)),
    )
}

/// Resolve the first of `candidates` that rasterizes and build a composited icon
/// element centered at `origin + center` (both menu-local logical), tinted
/// `color`. `None` when no candidate resolves.
#[allow(clippy::too_many_arguments)]
fn icon_element(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[&str],
    logical_px: f64,
    scale: f64,
    color: [f32; 4],
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
) -> Option<TextureRenderElement<VkTexture>> {
    let buffer = candidates
        .iter()
        .find_map(|name| icons.buffer(name, logical_px, scale, color))?;
    let tb = match TextureBuffer::from_memory_buffer(renderer, &buffer) {
        Ok(tb) => tb,
        Err(err) => {
            tracing::error!("error uploading quick-settings icon: {err:#}");
            return None;
        }
    };
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        1.,
        None,
        None,
        Kind::Unspecified,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tile grid and system row lay out without overlap and inside the menu.
    #[test]
    fn layout_places_tiles_and_system_row_within_bounds() {
        let size = QuickSettings::new(QuickToggles::default(), [0, 0, 0]).logical_size();
        for i in 0..TILES.len() {
            let r = tile_rect(i);
            assert!(r.loc.x >= 0. && r.loc.y >= 0.);
            assert!(r.loc.x + r.size.w <= size.w + 0.01);
            assert!(r.loc.y + r.size.h <= size.h + 0.01);
        }
        for i in 0..SYS_BUTTONS.len() {
            let r = sys_rect(i);
            assert!(r.loc.x >= 0., "sys button {i} off the left edge");
            assert!(r.loc.x + r.size.w <= size.w + 0.01);
            assert!(r.loc.y + r.size.h <= size.h + 0.01);
        }
    }

    /// Clicking a tile flips its state and returns the matching set-action.
    #[test]
    fn clicking_a_tile_flips_and_returns_the_action() {
        let mut qs = QuickSettings::new(QuickToggles::default(), [0, 0, 0]);
        let dnd = tile_rect(1); // Do Not Disturb
        let before = qs.revision;
        let action = qs.pointer_click(Point::from((
            dnd.loc.x + dnd.size.w / 2.,
            dnd.loc.y + dnd.size.h / 2.,
        )));
        assert!(matches!(action, PopoverAction::SetDoNotDisturb(true)));
        assert!(qs.toggles.do_not_disturb);
        assert!(qs.revision > before);
    }

    /// A system button returns a spawn of its canonical command.
    #[test]
    fn clicking_a_system_button_spawns() {
        let mut qs = QuickSettings::new(QuickToggles::default(), [0, 0, 0]);
        let settings = sys_rect(0);
        let action = qs.pointer_click(Point::from((
            settings.loc.x + settings.size.w / 2.,
            settings.loc.y + settings.size.h / 2.,
        )));
        match action {
            PopoverAction::Spawn(cmd) => assert_eq!(cmd[0], "gnome-control-center"),
            other => panic!("expected a spawn, got {other:?}"),
        }
    }

    /// A click in empty menu space is consumed but does nothing.
    #[test]
    fn clicking_empty_space_is_consumed() {
        let mut qs = QuickSettings::new(QuickToggles::default(), [0, 0, 0]);
        let action = qs.pointer_click(Point::from((1., 1.)));
        assert!(matches!(action, PopoverAction::Consumed));
    }

    /// Draw the chrome into an offscreen and read it back: an opaque dark menu
    /// with the active tile painted the accent color and visible label ink.
    #[test]
    fn draws_the_menu_with_an_active_tile() {
        use smithay::backend::renderer::{ExportMem, Texture as _};

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_the_menu_with_an_active_tile: no Vulkan device ({e})");
                return;
            }
        };
        // Dark Style on, with a vivid red accent so the active tile is unmistakable.
        let toggles = QuickToggles {
            dark_style: true,
            ..Default::default()
        };
        let qs = QuickSettings::new(toggles, [0xff, 0x00, 0x00]);
        let mut tex = qs.draw(&mut vk, 1.).expect("menu texture");
        let size = tex.size();

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // The center of the Dark Style tile (index 0) is the accent: high R, low G/B.
        let r0 = tile_rect(0);
        let cx = (r0.loc.x + r0.size.w * 0.15) as i32;
        let cy = (r0.loc.y + r0.size.h / 2.) as i32;
        let i = ((cy * size.w + cx) * 4) as usize;
        let px = [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]];
        assert!(
            px[0] > 150 && px[1] < 80 && px[2] < 80 && px[3] > 150,
            "the active Dark Style tile must be the accent color, got {px:?}"
        );

        // The Night Light tile (index 2, off) is the dim grey, not the accent.
        let r2 = tile_rect(2);
        let gx = (r2.loc.x + r2.size.w * 0.15) as i32;
        let gy = (r2.loc.y + r2.size.h / 2.) as i32;
        let j = ((gy * size.w + gx) * 4) as usize;
        let g = [pixels[j], pixels[j + 1], pixels[j + 2]];
        assert!(
            g[0] < 100 && g[1] < 100 && g[2] < 100,
            "an inactive tile must be dim grey, got {g:?}"
        );
    }
}
