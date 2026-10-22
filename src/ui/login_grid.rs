// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The login screen's user grid — the greeter's first page (`docs/fork/greeter.md` §4).
//!
//! **Divergence, by design:** gnome-shell 50.3's `LoginDialog` shows a vertical list
//! (`js/gdm/loginDialog.js:186-306`). This follows the GNOME design team's user-selection-grid
//! mockup instead: rounded tiles tinted from each avatar, four to a row, centred, at most twelve
//! users; past that, or with none, the page is just the name entry. What it lists, and when, is
//! still `UserList`'s: [`crate::dbus::accounts_service::login_list`].
//!
//! The geometry is the mockup's, measured at 1366x768.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::dbus::accounts_service::LoginUser;
use crate::image_source::ImageSource;
use crate::render_helpers::icon::{IconCache, ImageCache, ImageFit};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::lock_screen::{PageCtx, PageTransform, PromptElement};
use crate::ui::widget::{self, style, Align, HAlign, Painter, Rgba, TextShaper, TextStyle};

/// Tile side, logical px.
pub const TILE: f64 = 200.;
/// Space between tiles, both ways.
pub const GAP: f64 = 48.;
const TILE_RADIUS: f64 = 16.;
/// The circular picture, and where its centre sits below the tile's top.
pub const AVATAR_PX: f64 = 96.;
const AVATAR_CENTRE_Y: f64 = 80.;
/// The name's vertical centre below the tile's top, and its side inset.
const NAME_CENTRE_Y: f64 = 161.;
const NAME_INSET: f64 = 12.;
pub const MAX_COLUMNS: usize = 4;
/// "Grid continues to expand until there is no more room or a maximum of 12 users are displayed
/// (after this point, it is better for someone to enter a username)."
pub const MAX_USERS: usize = 12;
/// The "Enter User Name" pill's height.
const PILL_H: f64 = 40.;
const PILL_PAD: f64 = 18.;
const PILL_ICON_PX: f64 = 16.;
const PILL_ICON_GAP: f64 = 8.;
pub const ENTER_NAME_TEXT: &str = "Enter User Name";
const ENTER_NAME_ICON: &str = "go-next-symbolic";
/// A tile with no picture to take a tint from.
const TILE_NEUTRAL: Rgba = [0.235, 0.227, 0.251, 1.];
/// The tint's ceiling on luminance, so white names stay legible on a pale picture.
const TINT_MAX_LUMA: f32 = 0.42;
/// How much of the neutral tile goes into a tint.
const TINT_NEUTRAL_MIX: f32 = 0.4;
/// The avatar's placeholder disc behind the themed glyph.
const AVATAR_BG: Rgba = [1., 1., 1., 0.13];
const AVATAR_ICON_PAD: f64 = 18.;
/// The size the picture is sampled at for its tint.
const TINT_SAMPLE_PX: f64 = 8.;

/// What a point or the keyboard focus is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridHit {
    Tile(usize),
    EnterName,
    /// The dialog's accessibility button — a stop in the page's focus chain, after the pill. Never
    /// a pointer hit here: the button is on every page, so the dialog tracks its hover.
    A11y,
}

/// What the page asks of its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridAction {
    /// Ask this user for credentials.
    Pick(usize),
    /// Ask for a name.
    EnterName,
    /// Open the accessibility menu.
    A11y,
}

/// Whether there is a grid to show for `count` listed users.
pub fn shows_grid(count: usize) -> bool {
    (1..=MAX_USERS).contains(&count)
}

/// The tiles' rects for `count` users, in reading order: rows of [`MAX_COLUMNS`], each row centred,
/// the block centred on the monitor.
pub fn tile_rects(monitor: Rectangle<f64, Logical>, count: usize) -> Vec<Rectangle<f64, Logical>> {
    let count = count.min(MAX_USERS);
    if count == 0 {
        return Vec::new();
    }
    let rows = count.div_ceil(MAX_COLUMNS);
    let block_h = rows as f64 * TILE + (rows - 1) as f64 * GAP;
    let top = monitor.loc.y + (monitor.size.h - block_h) / 2.;
    (0..count)
        .map(|i| {
            let row = i / MAX_COLUMNS;
            let in_row = (count - row * MAX_COLUMNS).min(MAX_COLUMNS);
            let row_w = in_row as f64 * TILE + (in_row - 1) as f64 * GAP;
            let left = monitor.loc.x + (monitor.size.w - row_w) / 2.;
            let col = i % MAX_COLUMNS;
            Rectangle::new(
                Point::from((
                    left + col as f64 * (TILE + GAP),
                    top + row as f64 * (TILE + GAP),
                )),
                Size::from((TILE, TILE)),
            )
        })
        .collect()
}

/// The pill's width: padding, label, gap, chevron, padding.
fn pill_width() -> f64 {
    let pt = crate::ui::BASE_FONT_PT;
    let label = synoik_vk::text::measure_line_width_weighted(
        ENTER_NAME_TEXT,
        crate::ui::pt_to_px(pt) as f32,
        false,
    );
    PILL_PAD * 2. + label + PILL_ICON_GAP + PILL_ICON_PX
}

/// The "Enter User Name" pill at the monitor's bottom-right.
pub fn enter_name_rect(monitor: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    // In the dialog's bottom button group, before the accessibility button: GNOME's group holds
    // the session button there (`loginDialog.js:643-658`), which this greeter does not have. The
    // pill is shorter than the round button, so the two share a centre line.
    let w = pill_width();
    let a11y = crate::ui::login_dialog::a11y_rect(monitor);
    Rectangle::new(
        Point::from((
            a11y.loc.x - crate::ui::login_dialog::BUTTON_GROUP_SPACING - w,
            a11y.loc.y + (a11y.size.h - PILL_H) / 2.,
        )),
        Size::from((w, PILL_H)),
    )
}

/// What sits at `pos`.
pub fn hit(
    monitor: Rectangle<f64, Logical>,
    count: usize,
    pos: Point<f64, Logical>,
) -> Option<GridHit> {
    if !shows_grid(count) {
        return None;
    }
    if let Some(i) = tile_rects(monitor, count)
        .iter()
        .position(|r| r.contains(pos))
    {
        return Some(GridHit::Tile(i));
    }
    enter_name_rect(monitor)
        .contains(pos)
        .then_some(GridHit::EnterName)
}

/// The page's state: what the pointer and the keyboard are on.
#[derive(Default)]
pub struct LoginGrid {
    hovered: Option<GridHit>,
    focused: Option<GridHit>,
    /// One bake per tile, and the pill's. Indexed like the list.
    tiles: RefCell<Vec<widget::BakeCache>>,
    pill: RefCell<widget::BakeCache>,
    /// The pictures, one upload slot per tile index.
    avatars: RefCell<widget::ImageUploads>,
    rings: RefCell<Vec<widget::BakeCache>>,
}

impl LoginGrid {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hovered(&self) -> Option<GridHit> {
        self.hovered
    }

    pub fn focused(&self) -> Option<GridHit> {
        self.focused
    }

    /// Returns whether it changed, for the caller's redraw.
    pub fn set_hovered(&mut self, hovered: Option<GridHit>) -> bool {
        std::mem::replace(&mut self.hovered, hovered) != hovered
    }

    /// Put the focus on the first tile, as `UserList._moveFocusToItems` does when the list gets
    /// the key focus (`loginDialog.js:198-215`).
    pub fn reset(&mut self, count: usize) {
        self.hovered = None;
        self.focused = shows_grid(count).then_some(GridHit::Tile(0));
    }

    /// Drop a focus the list no longer has.
    pub fn clamp(&mut self, count: usize) {
        match self.focused {
            Some(GridHit::Tile(i)) if !shows_grid(count) || i >= count => {
                self.focused = shows_grid(count).then_some(GridHit::Tile(count.saturating_sub(1)));
            }
            _ => {}
        }
        if let Some(GridHit::Tile(i)) = self.hovered {
            if i >= count {
                self.hovered = None;
            }
        }
    }

    /// A key on the page. Arrows walk the grid, Tab and Shift+Tab walk the tiles, the pill and the
    /// accessibility button, Enter and space activate the focus. Returns what to do, if anything,
    /// and whether the focus moved.
    pub fn key(&mut self, key: GridKey, count: usize) -> (Option<GridAction>, bool) {
        if !shows_grid(count) {
            return (None, false);
        }
        let count = count.min(MAX_USERS);
        let before = self.focused;
        let stops = count + 2;
        let index = |h: Option<GridHit>| match h {
            Some(GridHit::Tile(i)) => Some(i),
            Some(GridHit::EnterName) => Some(count),
            Some(GridHit::A11y) => Some(count + 1),
            None => None,
        };
        let from_index = |i: usize| match i.checked_sub(count) {
            Some(0) => GridHit::EnterName,
            Some(_) => GridHit::A11y,
            None => GridHit::Tile(i),
        };
        let current = index(self.focused);
        match key {
            GridKey::Activate => {
                return (
                    self.focused.map(|f| match f {
                        GridHit::Tile(i) => GridAction::Pick(i),
                        GridHit::EnterName => GridAction::EnterName,
                        GridHit::A11y => GridAction::A11y,
                    }),
                    false,
                );
            }
            GridKey::Next => {
                let i = current.map_or(0, |i| (i + 1) % stops);
                self.focused = Some(from_index(i));
            }
            GridKey::Prev => {
                let i = current.map_or(stops - 1, |i| (i + stops - 1) % stops);
                self.focused = Some(from_index(i));
            }
            GridKey::Left | GridKey::Right => {
                if let Some(GridHit::Tile(i)) = self.focused {
                    let row_start = i / MAX_COLUMNS * MAX_COLUMNS;
                    let row_end = (row_start + MAX_COLUMNS).min(count);
                    let j = if key == GridKey::Left {
                        i.checked_sub(1).filter(|&j| j >= row_start)
                    } else {
                        Some(i + 1).filter(|&j| j < row_end)
                    };
                    if let Some(j) = j {
                        self.focused = Some(GridHit::Tile(j));
                    }
                } else if self.focused.is_none() {
                    self.focused = Some(GridHit::Tile(0));
                } else if key == GridKey::Right && self.focused == Some(GridHit::EnterName) {
                    self.focused = Some(GridHit::A11y);
                } else if key == GridKey::Left && self.focused == Some(GridHit::A11y) {
                    self.focused = Some(GridHit::EnterName);
                }
            }
            GridKey::Up => match self.focused {
                Some(GridHit::Tile(i)) if i >= MAX_COLUMNS => {
                    self.focused = Some(GridHit::Tile(i - MAX_COLUMNS));
                }
                Some(GridHit::EnterName | GridHit::A11y) => {
                    self.focused = Some(GridHit::Tile(count - 1))
                }
                None => self.focused = Some(GridHit::Tile(0)),
                _ => {}
            },
            GridKey::Down => match self.focused {
                Some(GridHit::Tile(i)) => {
                    let below = i + MAX_COLUMNS;
                    self.focused = Some(if below < count {
                        GridHit::Tile(below)
                    } else if i / MAX_COLUMNS < (count - 1) / MAX_COLUMNS {
                        // The last row is shorter: land on its last tile.
                        GridHit::Tile(count - 1)
                    } else {
                        GridHit::EnterName
                    });
                }
                None => self.focused = Some(GridHit::Tile(0)),
                Some(GridHit::EnterName | GridHit::A11y) => {}
            },
        }
        (None, self.focused != before)
    }

    /// A click on the page.
    pub fn click(
        &mut self,
        monitor: Rectangle<f64, Logical>,
        count: usize,
        pos: Point<f64, Logical>,
    ) -> Option<GridAction> {
        let h = hit(monitor, count, pos)?;
        self.focused = Some(h);
        Some(match h {
            GridHit::Tile(i) => GridAction::Pick(i),
            GridHit::EnterName => GridAction::EnterName,
            GridHit::A11y => GridAction::A11y,
        })
    }

    /// Front-to-back, like every other page render.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        images: &ImageCache,
        ctx: PageCtx,
        users: &[LoginUser],
        accent: [u8; 3],
        t: PageTransform,
    ) -> Vec<PromptElement> {
        let PageCtx { scale, monitor, .. } = ctx;
        let mut elements: Vec<PromptElement> = Vec::new();
        if !shows_grid(users.len()) {
            return elements;
        }
        let accent_rgba = style::accent_rgba(accent);
        let screen_centre = Point::from((
            monitor.loc.x + monitor.size.w / 2.,
            monitor.loc.y + monitor.size.h / 2.,
        ));

        // The pill first: it is above nothing, and nothing is above it.
        let pill = enter_name_rect(monitor);
        if let Some(el) = widget::icon_element_scaled(
            renderer,
            icons,
            &[ENTER_NAME_ICON],
            PILL_ICON_PX,
            scale,
            t.scale,
            style::TEXT,
            pill.loc,
            t.place(
                Point::from((
                    pill.loc.x + pill.size.w - PILL_PAD - PILL_ICON_PX / 2.,
                    pill.loc.y + PILL_H / 2.,
                )),
                screen_centre,
            ) - pill.loc,
            t.alpha as f32,
        ) {
            elements.push(el.into());
        }
        let hovered = self.hovered == Some(GridHit::EnterName);
        let focused = self.focused == Some(GridHit::EnterName);
        let pill_size = pill.size;
        match widget::bake(
            renderer,
            &mut self.pill.borrow_mut(),
            scale,
            pill_size,
            widget::Revision::new()
                .of(hovered)
                .of(focused)
                .of(accent)
                .px(pill_size.w)
                .done(),
            |renderer| {
                TextShaper::new(renderer, scale)
                    .shape(ENTER_NAME_TEXT, TextStyle::new(crate::ui::BASE_FONT_PT))
            },
            |frame, phys, label| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                let r = Rectangle::from_size(pill_size);
                p.fill_rounded(r, PILL_H / 2., style::SYSTEM_BUTTON_BG)?;
                if hovered {
                    p.fill_rounded(r, PILL_H / 2., style::HOVER_WASH)?;
                }
                if focused {
                    p.focus_ring(r, PILL_H / 2., accent_rgba)?;
                }
                p.text(
                    label,
                    Point::from((PILL_PAD, PILL_H / 2.)),
                    Align::LEFT_MIDDLE,
                    style::TEXT,
                )?;
                Ok(())
            },
        ) {
            Ok(texture) => {
                elements.push(element(texture, scale, pill.loc, t, screen_centre).into())
            }
            Err(err) => tracing::error!("error drawing the enter-name button: {err:#}"),
        }

        let rects = tile_rects(monitor, users.len());
        {
            let mut tiles = self.tiles.borrow_mut();
            tiles.resize_with(rects.len(), widget::BakeCache::new);
            let mut rings = self.rings.borrow_mut();
            rings.resize_with(rects.len(), widget::BakeCache::new);
        }
        let mut uploads = self.avatars.borrow_mut();
        let sources: Vec<Option<ImageSource>> = users
            .iter()
            .map(|u| {
                u.icon_file
                    .as_ref()
                    .map(|i| ImageSource::File(i.path.clone()))
            })
            .collect();
        // A slot holds what its tile shows now; anything else is a picture that left the list.
        uploads.retain_slots(|slot| (slot as usize) < rects.len());
        uploads.retain_sources(|source| sources.iter().flatten().any(|s| s == source));

        for (i, (user, rect)) in users.iter().zip(&rects).enumerate() {
            let source = sources[i].as_ref();
            let avatar_centre = t.place(
                Point::from((rect.loc.x + TILE / 2., rect.loc.y + AVATAR_CENTRE_Y)),
                screen_centre,
            ) - rect.loc;

            // The picture's ring: accent for a user with a session (`:logged-in`,
            // `_login-lock.scss:200-207`), else the plain inset ring.
            let picture = source.and_then(|source| {
                widget::Avatar::element(
                    renderer,
                    &mut uploads,
                    images,
                    source,
                    i as u64,
                    AVATAR_PX,
                    scale,
                    t.scale,
                    rect.loc,
                    avatar_centre,
                    t.alpha as f32,
                )
            });
            let ring_color = if user.logged_in {
                accent_rgba
            } else {
                widget::Avatar::RING_COLOR
            };
            if picture.is_some() || user.logged_in {
                let avatar_loc = Point::from((
                    rect.loc.x + (TILE - AVATAR_PX) / 2.,
                    rect.loc.y + AVATAR_CENTRE_Y - AVATAR_PX / 2.,
                ));
                match widget::bake_card_border(
                    renderer,
                    &mut self.rings.borrow_mut()[i],
                    scale,
                    widget::Revision::new()
                        .px(AVATAR_PX)
                        .of(user.logged_in)
                        .of(accent)
                        .done(),
                    Size::from((AVATAR_PX, AVATAR_PX)),
                    AVATAR_PX / 2.,
                    ring_color,
                ) {
                    Ok(texture) => {
                        elements.push(element(texture, scale, avatar_loc, t, screen_centre).into())
                    }
                    Err(err) => tracing::error!("error drawing an avatar ring: {err:#}"),
                }
            }
            match picture {
                Some(el) => elements.push(PromptElement::Rounded(el)),
                None => {
                    if let Some(el) = widget::icon_element_scaled(
                        renderer,
                        icons,
                        &["avatar-default-symbolic"],
                        AVATAR_PX - AVATAR_ICON_PAD * 2.,
                        scale,
                        t.scale,
                        style::TEXT,
                        rect.loc,
                        avatar_centre,
                        t.alpha as f32,
                    ) {
                        elements.push(el.into());
                    }
                }
            }

            // The tile: tint, placeholder disc, name, hover and focus.
            let tint = source.map_or(TILE_NEUTRAL, |s| tint_of(images, s, scale));
            let hovered = self.hovered == Some(GridHit::Tile(i));
            let focused = self.focused == Some(GridHit::Tile(i));
            let has_picture = source.is_some();
            let name = widget::ellipsized_line(
                user.display_name(),
                crate::ui::BASE_FONT_PT,
                TILE - 2. * NAME_INSET,
            );
            let tint_key = tint.map(|c| (c * 255.) as u8);
            match widget::bake(
                renderer,
                &mut self.tiles.borrow_mut()[i],
                scale,
                Size::from((TILE, TILE)),
                widget::Revision::new()
                    .of(&name)
                    .of(tint_key)
                    .of(hovered)
                    .of(focused)
                    .of(has_picture)
                    .of(accent)
                    .done(),
                |renderer| {
                    TextShaper::new(renderer, scale)
                        .shape(&name, TextStyle::new(crate::ui::BASE_FONT_PT))
                },
                |frame, phys, label| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    let r = Rectangle::from_size(Size::from((TILE, TILE)));
                    p.fill_rounded(r, TILE_RADIUS, tint)?;
                    if hovered {
                        p.fill_rounded(r, TILE_RADIUS, style::HOVER_WASH)?;
                    }
                    if focused {
                        p.focus_ring(r, TILE_RADIUS, accent_rgba)?;
                    }
                    if !has_picture {
                        let disc = Rectangle::new(
                            Point::from((
                                (TILE - AVATAR_PX) / 2.,
                                AVATAR_CENTRE_Y - AVATAR_PX / 2.,
                            )),
                            Size::from((AVATAR_PX, AVATAR_PX)),
                        );
                        p.fill_rounded(disc, AVATAR_PX / 2., AVATAR_BG)?;
                    }
                    // Centred on the line box, not the ink, so names with and without descenders
                    // share a baseline.
                    let band_h = label.line_box_height() as f64 / scale;
                    p.text_band(
                        label,
                        TILE / 2.,
                        HAlign::Center,
                        NAME_CENTRE_Y - band_h / 2.,
                        band_h,
                        style::TEXT,
                        Rectangle::new(
                            Point::from((NAME_INSET, 0.)),
                            Size::from((TILE - 2. * NAME_INSET, TILE)),
                        ),
                    )?;
                    Ok(())
                },
            ) {
                Ok(texture) => {
                    elements.push(element(texture, scale, rect.loc, t, screen_centre).into())
                }
                Err(err) => tracing::error!("error drawing a user tile: {err:#}"),
            }
        }
        elements
    }
}

/// The keys the page understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridKey {
    Left,
    Right,
    Up,
    Down,
    Next,
    Prev,
    Activate,
}

/// A tile's colour from its picture: the picture's mean, darkened until white text reads on it.
/// Neutral until the picture has loaded.
fn tint_of(images: &ImageCache, source: &ImageSource, scale: f64) -> Rgba {
    let Some(buffer) = images.buffer(source, ImageFit::Cover, TINT_SAMPLE_PX, scale) else {
        return TILE_NEUTRAL;
    };
    mean_tint(buffer.data()).unwrap_or(TILE_NEUTRAL)
}

/// The mean colour of premultiplied RGBA bytes, muted toward [`TILE_NEUTRAL`] and darkened to
/// [`TINT_MAX_LUMA`].
pub fn mean_tint(rgba: &[u8]) -> Option<Rgba> {
    let (mut r, mut g, mut b, mut a) = (0u64, 0u64, 0u64, 0u64);
    for px in rgba.as_chunks::<4>().0 {
        r += u64::from(px[0]);
        g += u64::from(px[1]);
        b += u64::from(px[2]);
        a += u64::from(px[3]);
    }
    if a == 0 {
        return None;
    }
    // Premultiplied: dividing by the summed alpha un-premultiplies the mean.
    let mut c = [
        r as f32 / a as f32,
        g as f32 / a as f32,
        b as f32 / a as f32,
    ];
    // Muted, as the mockup's are: part picture, part neutral tile.
    for (v, n) in c.iter_mut().zip(TILE_NEUTRAL) {
        *v = *v * (1. - TINT_NEUTRAL_MIX) + n * TINT_NEUTRAL_MIX;
    }
    let luma = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    if luma > TINT_MAX_LUMA {
        let k = TINT_MAX_LUMA / luma;
        c = c.map(|v| v * k);
    }
    Some([c[0], c[1], c[2], 1.])
}

fn element(
    texture: TextureBuffer<VkTexture>,
    scale: f64,
    loc: Point<f64, Logical>,
    t: PageTransform,
    centre: Point<f64, Logical>,
) -> TextureRenderElement<VkTexture> {
    let mut buffer = texture;
    buffer.set_texture_scale(t.buffer_scale(scale));
    TextureRenderElement::from_texture_buffer(
        buffer,
        t.place(loc, centre),
        t.alpha as f32,
        None,
        None,
        Kind::Unspecified,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon() -> Rectangle<f64, Logical> {
        Rectangle::from_size(Size::from((1366., 768.)))
    }

    /// The mockup's eight-user frame: two rows of four, tiles at x 211/459/707/955, y 160/408.
    #[test]
    fn eight_users_land_where_the_mockup_puts_them() {
        let r = tile_rects(mon(), 8);
        let xs: Vec<_> = r[..4].iter().map(|r| r.loc.x).collect();
        assert_eq!(xs, [211., 459., 707., 955.]);
        assert_eq!(r[0].loc.y, 160.);
        assert_eq!(r[4].loc.y, 408.);
    }

    /// A short last row is centred on its own, and one user sits in the middle of the screen.
    #[test]
    fn short_rows_centre() {
        let r = tile_rects(mon(), 5);
        assert_eq!(r[4].loc.x, (1366. - TILE) / 2.);
        let one = tile_rects(mon(), 1);
        assert_eq!(
            one[0].loc,
            Point::from(((1366. - TILE) / 2., (768. - TILE) / 2.))
        );
        assert!(tile_rects(mon(), 13).len() <= MAX_USERS);
        assert!(!shows_grid(13) && !shows_grid(0));
    }

    /// Arrows keep to their row and column; Down off the last row reaches the pill; Tab cycles
    /// through the pill and back.
    #[test]
    fn the_keyboard_walks_the_grid() {
        let mut g = LoginGrid::new();
        g.reset(6);
        assert_eq!(g.focused(), Some(GridHit::Tile(0)));
        g.key(GridKey::Left, 6);
        assert_eq!(g.focused(), Some(GridHit::Tile(0)));
        g.key(GridKey::Right, 6);
        g.key(GridKey::Right, 6);
        g.key(GridKey::Right, 6);
        g.key(GridKey::Right, 6);
        assert_eq!(
            g.focused(),
            Some(GridHit::Tile(3)),
            "a row ends at its last tile"
        );
        g.key(GridKey::Down, 6);
        assert_eq!(
            g.focused(),
            Some(GridHit::Tile(5)),
            "onto the short row's last tile"
        );
        g.key(GridKey::Down, 6);
        assert_eq!(g.focused(), Some(GridHit::EnterName));
        assert_eq!(g.key(GridKey::Activate, 6).0, Some(GridAction::EnterName));
        g.key(GridKey::Right, 6);
        assert_eq!(g.focused(), Some(GridHit::A11y), "the pill's neighbour");
        assert_eq!(g.key(GridKey::Activate, 6).0, Some(GridAction::A11y));
        g.key(GridKey::Left, 6);
        assert_eq!(g.focused(), Some(GridHit::EnterName));
        g.key(GridKey::Next, 6);
        assert_eq!(g.focused(), Some(GridHit::A11y));
        g.key(GridKey::Next, 6);
        assert_eq!(g.focused(), Some(GridHit::Tile(0)));
        g.key(GridKey::Prev, 6);
        assert_eq!(g.focused(), Some(GridHit::A11y));
    }

    /// Pale pictures are darkened for the white name; an empty one has no tint.
    #[test]
    fn the_tint_keeps_names_legible() {
        let white = [255u8; 4 * 4];
        let t = mean_tint(&white).unwrap();
        let luma = 0.2126 * t[0] + 0.7152 * t[1] + 0.0722 * t[2];
        assert!((luma - TINT_MAX_LUMA).abs() < 1e-4);
        assert_eq!(mean_tint(&[0u8; 16]), None);
    }
}
