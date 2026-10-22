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

use crate::osk::layout::{Face, Level};
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

/// Each key's cap, keyboard-local, in the level's key order.
pub fn key_rects(size: Size<f64, Logical>, level: &Level) -> Vec<Rectangle<f64, Logical>> {
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
            Rectangle::new(
                cell.loc + Point::from((KEY_PADDING, KEY_PADDING)),
                Size::from((
                    (cell.size.w - 2. * KEY_PADDING).max(0.),
                    (cell.size.h - 2. * KEY_PADDING).max(0.),
                )),
            )
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

/// The drawn keyboard's cache.
#[derive(Default)]
pub struct OskUi {
    cache: RefCell<ContentCache>,
}

impl std::fmt::Debug for OskUi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OskUi").finish_non_exhaustive()
    }
}

struct Prepared {
    caps: Vec<KeyCap>,
    labels: Vec<Option<ShapedText>>,
    size: Size<f64, Logical>,
}

impl OskUi {
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
                    || key.keyval.is_some_and(|k| osk.modifier_latched(k)),
            })
            .collect();

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
                    let labels = level
                        .keys
                        .iter()
                        .map(|key| match &key.face {
                            Face::Label(label) if !label.is_empty() => {
                                shaper.shape(label, KeyCap::text_style()).ok()
                            }
                            _ => None,
                        })
                        .collect();
                    let px = |v: f64| to_physical_precise_round::<i32>(scale, v).max(1);
                    let phys = Size::<i32, Physical>::from((px(rect.size.w), px(rect.size.h)));
                    Ok((
                        phys,
                        Prepared {
                            caps,
                            labels,
                            size: rect.size,
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
    for (cap, label) in prepared.caps.iter().zip(&prepared.labels) {
        p.key_cap(cap, label.as_ref(), accent)?;
    }
    Ok(())
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
