// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Drawing and hit-testing the on-screen keyboard ([`crate::osk`]).
//!
//! `#keyboard` is a vertical box: the word suggestions on top, then an `AspectContainer` holding
//! the current level's key grid at the level's aspect ratio, centred across the width
//! (`keyboard.js:34-119,1260-1299`). Every key sits in a `.key-container` cell with a 4px padding
//! (`_keyboard.scss:24-28`).
//!
//! The panel and its labelled keys are one bake; icon keys composite their glyph on top.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::osk::layout::{Action, Face, Key, Level};
use crate::osk::Osk;
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::widget::{self, style, ContentCache, KeyCap, Painter, ShapedText, TextShaper};
use crate::utils::to_physical_precise_round;

/// `.key-container { padding: $base_margin }`.
const KEY_PADDING: f64 = 4.;

/// `.word-suggestions`: `min-height: 17pt`, `padding: $base_padding * 2`.
fn suggestions_height() -> f64 {
    crate::ui::pt_to_px(17.) + 2. * 12.
}

/// The keyboard's minimum height (`get_preferred_height`): the suggestions, and four rows of
/// keys no shorter than their labels.
pub fn min_height() -> f64 {
    let key = crate::ui::line_height_px(KeyCap::text_style().pt) + 2. * KEY_PADDING;
    suggestions_height() + 4. * key
}

/// The rectangle the key grid fills, keyboard-local: below the suggestions, and no wider than
/// the level's ratio allows (`AspectContainer.vfunc_allocate`).
fn grid_area(size: Size<f64, Logical>, level: &Level) -> Rectangle<f64, Logical> {
    let top = suggestions_height().min(size.h);
    let mut area = Rectangle::new(
        Point::from((0., top)),
        Size::from((size.w, (size.h - top).max(0.))),
    );
    if area.size.w > 0. && area.size.h > 0. && level.rows > 0 {
        let ratio = level.cols / f64::from(level.rows);
        if area.size.w / area.size.h >= ratio {
            let width = area.size.h * ratio;
            let diff = area.size.w - width;
            area.loc.x += (diff / 2.).floor();
            area.size.w -= (diff / 2.).floor() + (diff / 2.).ceil();
        }
    }
    area
}

/// The emoji page's dots (`.page-indicator`, `_keyboard.scss:14-21`): an 8px dot in a 6px
/// padding.
const DOT: f64 = 8.;
const DOT_PADDING: f64 = 6.;
const DOT_CELL: f64 = DOT + 2. * DOT_PADDING;

/// The emoji page's pieces, keyboard-local: the pager its emoji fill and a swipe is clipped to,
/// the dots strip under it, and the bottom row (`EmojiSelection`: a grid with the pager box over
/// `rows - 1` rows and the bottom row in the last; the box stacks the pager over the dots).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmojiGeometry {
    pub pager: Rectangle<f64, Logical>,
    pub dots: Option<Rectangle<f64, Logical>>,
    pub bottom: Rectangle<f64, Logical>,
}

pub fn emoji_geometry(size: Size<f64, Logical>, level: &Level) -> Option<EmojiGeometry> {
    let pager = level.pager?;
    let area = grid_area(size, level);
    if level.rows == 0 {
        return None;
    }
    let row_h = area.size.h / f64::from(level.rows);
    let box_h = row_h * f64::from(level.rows - 1);
    let dots_h = if pager.indicators { DOT_CELL } else { 0. };
    let pager_rect = Rectangle::new(
        area.loc,
        Size::from((area.size.w, (box_h - dots_h).max(0.))),
    );
    let dots = pager.indicators.then(|| {
        Rectangle::new(
            Point::from((area.loc.x, area.loc.y + box_h - dots_h)),
            Size::from((area.size.w, dots_h)),
        )
    });
    // The bottom row's `AspectContainer` at `(nCols, 1)`.
    let mut bottom = Rectangle::new(
        Point::from((area.loc.x, area.loc.y + box_h)),
        Size::from((area.size.w, row_h)),
    );
    let width = (row_h * level.cols).min(bottom.size.w);
    let diff = bottom.size.w - width;
    bottom.loc.x += (diff / 2.).floor();
    bottom.size.w = width;
    Some(EmojiGeometry {
        pager: pager_rect,
        dots,
        bottom,
    })
}

/// Where the page dots go in the strip, and how big each is drawn: `max(1 - |position - i|, 0)`
/// of the way from inactive to active (`PageIndicators._updateIndicator`) — two thirds the size
/// and half the opacity at rest.
pub fn emoji_dots(
    strip: Rectangle<f64, Logical>,
    n: usize,
    position: f64,
) -> Vec<(Point<f64, Logical>, f64)> {
    let total = DOT_CELL * n as f64;
    let left = strip.loc.x + (strip.size.w - total) / 2.;
    (0..n)
        .map(|i| {
            let center = Point::from((
                left + DOT_CELL * (i as f64 + 0.5),
                strip.loc.y + DOT_CELL / 2.,
            ));
            let progress = (1. - (position - i as f64).abs()).max(0.);
            (center, progress)
        })
        .collect()
}

fn cap_of(cell: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        cell.loc + Point::from((KEY_PADDING, KEY_PADDING)),
        Size::from((
            (cell.size.w - 2. * KEY_PADDING).max(0.),
            (cell.size.h - 2. * KEY_PADDING).max(0.),
        )),
    )
}

/// Each key's cap, keyboard-local, in the level's key order.
pub fn key_rects(size: Size<f64, Logical>, level: &Level) -> Vec<Rectangle<f64, Logical>> {
    if let (Some(pager), Some(geometry)) = (level.pager, emoji_geometry(size, level)) {
        // The emoji in the pager's homogeneous grid; the bottom row in key units across its
        // own box.
        let cw = geometry.pager.size.w / f64::from(pager.cols.max(1));
        let ch = geometry.pager.size.h / f64::from(pager.rows.max(1));
        let unit = geometry.bottom.size.w / pager.bottom_units;
        return level
            .keys
            .iter()
            .map(|key| {
                let cell = if key.row < pager.rows {
                    Rectangle::new(
                        geometry.pager.loc + Point::from((key.col * cw, f64::from(key.row) * ch)),
                        Size::from((cw, ch)),
                    )
                } else {
                    Rectangle::new(
                        geometry.bottom.loc + Point::from((key.col * unit, 0.)),
                        Size::from((key.width * unit, geometry.bottom.size.h)),
                    )
                };
                cap_of(cell)
            })
            .collect();
    }
    let area = grid_area(size, level);
    if level.cols <= 0. || level.rows == 0 {
        return Vec::new();
    }
    let cw = area.size.w / level.cols;
    let ch = area.size.h / f64::from(level.rows);
    level
        .keys
        .iter()
        .map(|key| {
            let cell = Rectangle::new(
                Point::from((
                    area.loc.x + key.col * cw,
                    area.loc.y + f64::from(key.row) * ch,
                )),
                Size::from((key.width * cw, key.height * ch)),
            );
            cap_of(cell)
        })
        .collect()
}

/// The key under a keyboard-local point. The padding between caps belongs to the key whose cell
/// it is, as a `.key-container`'s padding does.
pub fn key_at(size: Size<f64, Logical>, level: &Level, p: Point<f64, Logical>) -> Option<usize> {
    let pad = Point::from((KEY_PADDING, KEY_PADDING));
    key_rects(size, level).iter().position(|rect| {
        let cell = Rectangle::new(
            rect.loc - pad,
            rect.size + Size::from((2. * KEY_PADDING, 2. * KEY_PADDING)),
        );
        cell.contains(p)
    })
}

/// `.keyboard-subkeys-boxpointer` (`_keyboard.scss:65-80`).
mod subkeys_style {
    pub const RADIUS: f64 = 22.;
    pub const ARROW_BASE: f64 = 20.;
    pub const ARROW_RISE: f64 = 10.;
    /// `-boxpointer-gap`: between the arrow's tip and the key.
    pub const GAP: f64 = 4.;
    /// Its `.key-container`: `padding: $base_padding * 2; spacing: $base_margin * 2`.
    pub const PADDING: f64 = 12.;
    pub const SPACING: f64 = 8.;
    /// `-arrow-border-width`.
    pub const BORDER: f64 = 1.;
}

/// Where a key's long-press popup goes, global: its box, the tip of its arrow, and each of its
/// keys — sized as the key it came from (`Key._getExtendedKeys`).
#[derive(Debug, Clone, PartialEq)]
pub struct SubkeysGeometry {
    pub rect: Rectangle<f64, Logical>,
    pub arrow_x: f64,
    pub keys: Vec<Rectangle<f64, Logical>>,
}

/// `BoxPointer.setPosition(keyButton, 0.5)`: centred over the key, its arrow on the key's top
/// centre, kept inside the display.
pub fn subkeys_geometry(
    cap: Rectangle<f64, Logical>,
    count: usize,
    bounds: Rectangle<f64, Logical>,
) -> SubkeysGeometry {
    use subkeys_style::*;
    let n = count as f64;
    let inset = BORDER + PADDING;
    let size = Size::from((
        2. * inset + n * cap.size.w + (n - 1.).max(0.) * SPACING,
        2. * inset + cap.size.h,
    ));
    let arrow_x = cap.loc.x + cap.size.w / 2.;
    let x = (arrow_x - size.w / 2.)
        .min(bounds.loc.x + bounds.size.w - size.w)
        .max(bounds.loc.x);
    let y = cap.loc.y - GAP - ARROW_RISE - size.h;
    let rect = Rectangle::new(Point::from((x, y)), size);
    let keys = (0..count)
        .map(|i| {
            Rectangle::new(
                rect.loc + Point::from((inset + i as f64 * (cap.size.w + SPACING), inset)),
                cap.size,
            )
        })
        .collect();
    SubkeysGeometry {
        rect,
        arrow_x,
        keys,
    }
}

/// The drawn keyboard's cache.
#[derive(Default)]
pub struct OskUi {
    cache: RefCell<ContentCache>,
    subkeys_cache: RefCell<ContentCache>,
}

impl std::fmt::Debug for OskUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OskUi").finish_non_exhaustive()
    }
}

struct Prepared {
    caps: Vec<KeyCap>,
    labels: Vec<Option<ShapedText>>,
    /// `.emoji-page` keys: a glyph, no cap.
    flat: Vec<bool>,
    size: Size<f64, Logical>,
    emoji: Option<PreparedEmoji>,
}

/// The emoji page's moving parts: the pager's clip and offset, the page sliding in beside it,
/// and the dots.
struct PreparedEmoji {
    pager: Rectangle<f64, Logical>,
    delta: f64,
    following: Vec<(Rectangle<f64, Logical>, Option<ShapedText>)>,
    following_offset: f64,
    dots: Vec<(Point<f64, Logical>, f64)>,
}

/// The em size an emoji is drawn at: the key's font size.
fn emoji_px() -> f64 {
    crate::ui::pt_to_px(KeyCap::text_style().pt)
}

struct PreparedSubkeys {
    rect: Rectangle<f64, Logical>,
    arrow_x: f64,
    caps: Vec<KeyCap>,
    labels: Vec<Option<ShapedText>>,
}

impl OskUi {
    /// Draw a key's long-press popup, `geometry` display-local.
    #[allow(clippy::too_many_arguments)]
    pub fn render_subkeys(
        &self,
        renderer: &mut VulkanRenderer,
        osk: &Osk,
        geometry: &SubkeysGeometry,
        scale: f64,
        accent: [u8; 3],
        push: &mut dyn FnMut(TextureRenderElement<VkTexture>),
    ) {
        let Some(subkeys) = osk.subkeys() else {
            return;
        };
        let emoji_strings = osk
            .level()
            .and_then(|level| level.keys.get(subkeys.key))
            .is_some_and(|key| matches!(key.face, Face::Emoji(_)));
        let accent = style::accent_rgba(accent);
        let origin = geometry
            .rect
            .loc
            .to_physical_precise_round(scale)
            .to_logical(scale);
        let local = |r: Rectangle<f64, Logical>| Rectangle::new(r.loc - geometry.rect.loc, r.size);
        let caps: Vec<KeyCap> = geometry
            .keys
            .iter()
            .enumerate()
            .map(|(i, rect)| KeyCap {
                rect: local(*rect),
                default_key: false,
                hovered: subkeys.hovered == Some(i),
                active: subkeys.pressed.is_some_and(|(p, _)| p == i),
                latched: false,
            })
            .collect();
        let size = Size::<f64, Logical>::from((
            geometry.rect.size.w,
            geometry.rect.size.h + subkeys_style::ARROW_RISE,
        ));
        let revision = widget::Revision::new()
            .of(osk.revision())
            .px(size.w)
            .px(geometry.arrow_x - geometry.rect.loc.x)
            .of(accent.map(f32::to_bits))
            .done();
        let baked = {
            let mut cache = self.subkeys_cache.borrow_mut();
            widget::bake_content(
                renderer,
                &mut cache,
                scale,
                revision,
                |renderer| {
                    let mut shaper = TextShaper::new(renderer, scale);
                    let labels = subkeys
                        .strings
                        .iter()
                        .map(|s| {
                            if emoji_strings {
                                shaper.shape_emoji(s, emoji_px()).ok()
                            } else {
                                shaper.shape(s, KeyCap::text_style()).ok()
                            }
                        })
                        .collect();
                    let px = |v: f64| to_physical_precise_round::<i32>(scale, v).max(1);
                    let phys = Size::<i32, Physical>::from((px(size.w), px(size.h)));
                    Ok((
                        phys,
                        PreparedSubkeys {
                            rect: Rectangle::from_size(geometry.rect.size),
                            arrow_x: geometry.arrow_x - geometry.rect.loc.x,
                            caps,
                            labels,
                        },
                    ))
                },
                |frame, phys, prepared| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    // `-arrow-background-color: $osd_bg_color`,
                    // `-arrow-border-color: lighten($osd_bg_color, 9%)`.
                    p.box_pointer_down(
                        prepared.rect,
                        prepared.arrow_x,
                        subkeys_style::RADIUS,
                        subkeys_style::ARROW_BASE,
                        subkeys_style::ARROW_RISE,
                        style::osd_bg(),
                        style::lighten(style::osd_bg(), 0.09),
                    )?;
                    for (cap, label) in prepared.caps.iter().zip(&prepared.labels) {
                        p.key_cap(cap, label.as_ref(), accent)?;
                    }
                    Ok(())
                },
            )
        };
        match baked {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
                push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => warn!("error drawing the on-screen keyboard's popup: {err:#}"),
        }
    }

    /// Draw the keyboard at `rect`, display-local, faded by its slide.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        osk: &Osk,
        rect: Rectangle<f64, Logical>,
        scale: f64,
        accent: [u8; 3],
        push: &mut dyn FnMut(TextureRenderElement<VkTexture>),
    ) {
        let Some(level) = osk.level() else {
            return;
        };
        let _span = tracy_client::span!("OskUi::render");
        let accent = style::accent_rgba(accent);
        let alpha = osk.progress().clamp(0., 1.) as f32;
        // Whole physical pixels, so the bake lands on the grid it was drawn for.
        let origin = rect.loc.to_physical_precise_round(scale).to_logical(scale);
        let rects = key_rects(rect.size, level);

        let section = osk.emoji_section();
        let caps: Vec<KeyCap> = level
            .keys
            .iter()
            .zip(&rects)
            .enumerate()
            .map(|(i, (key, rect))| KeyCap {
                rect: *rect,
                default_key: key.default_key,
                hovered: osk.hovered() == Some(i),
                active: osk.pressed().is_some_and(|p| p.key == i),
                latched: (key.is_shift() && osk.latched())
                    || key.keyval.is_some_and(|k| osk.modifier_latched(k))
                    || matches!(key.action, Some(Action::EmojiSection(s)) if Some(s) == section),
            })
            .collect();
        let flat: Vec<bool> = level.keys.iter().map(|k| k.flat).collect();
        let emoji_geometry = emoji_geometry(rect.size, level);

        // Glyphs over the bake, topmost first.
        for ((key, cap), latched) in level
            .keys
            .iter()
            .zip(&caps)
            .zip(caps.iter().map(|c| c.latched))
        {
            let Face::Icon(name) = &key.face else {
                continue;
            };
            // `_setCurrentLevelLatched`: a latched level's shift keys turn into caps lock.
            let name = if key.is_shift() && latched {
                "osk-caps-lock-symbolic"
            } else {
                name.as_str()
            };
            let center = cap.rect.loc + cap.rect.size.downscale(2.).to_point();
            if let Some(elem) = widget::icon_element_alpha(
                renderer,
                icons,
                &[name],
                KeyCap::icon_size(),
                scale,
                style::TEXT,
                origin,
                center,
                alpha,
            ) {
                push(elem);
            }
        }

        let revision = widget::Revision::new()
            .of(osk.revision())
            .px(rect.size.w)
            .px(rect.size.h)
            .of(accent.map(f32::to_bits))
            .done();
        let baked = {
            let mut cache = self.cache.borrow_mut();
            widget::bake_content(
                renderer,
                &mut cache,
                scale,
                revision,
                |renderer| {
                    let mut shaper = TextShaper::new(renderer, scale);
                    let mut shape = |key: &Key| match &key.face {
                        Face::Label(label) if !label.is_empty() => {
                            shaper.shape(label, KeyCap::text_style()).ok()
                        }
                        Face::Emoji(emoji) => shaper.shape_emoji(emoji, emoji_px()).ok(),
                        _ => None,
                    };
                    let labels = level.keys.iter().map(&mut shape).collect();
                    let emoji = emoji_geometry.and_then(|geometry| {
                        let (delta, following) = osk.emoji_slide()?;
                        let width = geometry.pager.size.w;
                        let following = following
                            .map(|level| {
                                key_rects(rect.size, &level)
                                    .into_iter()
                                    .zip(&level.keys)
                                    .filter(|(_, key)| key.flat)
                                    .map(|(cap, key)| (cap, shape(key)))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let following_offset = if delta < 0. {
                            width + crate::osk::EMOJI_PAGE_SEPARATION
                        } else {
                            -width - crate::osk::EMOJI_PAGE_SEPARATION
                        };
                        let dots = match (geometry.dots, osk.emoji_indicators(width)) {
                            (Some(strip), Some((n, position))) => emoji_dots(strip, n, position),
                            _ => Vec::new(),
                        };
                        Some(PreparedEmoji {
                            pager: geometry.pager,
                            delta,
                            following,
                            following_offset,
                            dots,
                        })
                    });
                    let px = |v: f64| to_physical_precise_round::<i32>(scale, v).max(1);
                    let phys = Size::<i32, Physical>::from((px(rect.size.w), px(rect.size.h)));
                    Ok((
                        phys,
                        Prepared {
                            caps,
                            labels,
                            flat,
                            size: rect.size,
                            emoji,
                        },
                    ))
                },
                |frame, phys, prepared| paint(frame, phys, prepared, scale, accent),
            )
        };
        match baked {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
                push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => warn!("error drawing the on-screen keyboard: {err:#}"),
        }
    }
}

fn paint(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    prepared: &Prepared,
    scale: f64,
    accent: widget::Rgba,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    // `#keyboard`: the OSD background, and `box-shadow: inset 0 1px 0 0 $osd_outer_borders_color`.
    p.clear(style::osd_bg())?;
    p.hairline(
        Rectangle::new(Point::from((0., 0.)), Size::from((prepared.size.w, 1.))),
        style::over(style::osd_bg(), style::osd_border()),
    )?;
    let emoji = prepared.emoji.as_ref();
    for ((cap, label), &flat) in prepared
        .caps
        .iter()
        .zip(&prepared.labels)
        .zip(&prepared.flat)
    {
        if !flat {
            p.key_cap(cap, label.as_ref(), accent)?;
            continue;
        }
        // `.emoji-page .keyboard-key`: transparent, borderless, in every state — the rule comes
        // after `:hover` and `:active` at the same specificity. The pager clips its pages.
        if let (Some(label), Some(emoji)) = (label, emoji) {
            emoji_glyph(&mut p, label, cap.rect, emoji.delta, emoji.pager)?;
        }
    }
    if let Some(emoji) = emoji {
        for (rect, label) in &emoji.following {
            if let Some(label) = label {
                let offset = emoji.delta + emoji.following_offset;
                emoji_glyph(&mut p, label, *rect, offset, emoji.pager)?;
            }
        }
        for &(center, progress) in &emoji.dots {
            p.page_dot(center, DOT, progress)?;
        }
    }
    Ok(())
}

fn emoji_glyph(
    p: &mut Painter,
    label: &ShapedText,
    cap: Rectangle<f64, Logical>,
    offset: f64,
    clip: Rectangle<f64, Logical>,
) -> anyhow::Result<()> {
    let Some(clip) = clip.intersection(Rectangle::new(
        cap.loc + Point::from((offset, 0.)),
        cap.size,
    )) else {
        return Ok(());
    };
    p.text_band(
        label,
        cap.loc.x + offset + cap.size.w / 2.,
        widget::HAlign::Center,
        cap.loc.y,
        cap.size.h,
        style::TEXT,
        clip,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::osk::layout::{Layout, Purpose};

    #[test]
    fn the_grid_keeps_its_ratio_and_centres() {
        let layout = Layout::load("us", Purpose::Normal).unwrap();
        let level = layout.level("default").unwrap();
        let size = Size::from((1920., 360.));
        let area = grid_area(size, level);
        let ratio = area.size.w / area.size.h;
        assert!((ratio - 11.5 / 4.).abs() < 0.01, "{ratio}");
        assert!((area.loc.x * 2. + area.size.w - 1920.).abs() <= 1.);

        let rects = key_rects(size, level);
        let q = rects[0];
        assert_eq!(q.loc.x, area.loc.x + KEY_PADDING);
        assert_eq!(key_at(size, level, q.loc), Some(0));
        // The padding is the key's: just left of the cap, still inside its cell.
        assert_eq!(key_at(size, level, q.loc - Point::from((2., 0.))), Some(0));
        // Above the grid, over the suggestions, is no key.
        assert_eq!(key_at(size, level, Point::from((960., 5.))), None);
    }
}
