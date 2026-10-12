// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The login screen's accessibility menu (`A11yMenuButton`, `js/gdm/loginDialog.js:415-459`): a
//! two-column `QuickSettingsMenu` of the ten accessibility quick toggles
//! (`js/ui/status/accessibility.js:148-330`), in that construction order.
//!
//! Unlike the panel's [`A11yMenu`](crate::ui::a11y_menu::A11yMenu), whose rows are switch menu
//! items, these are `QuickToggle`s: flipping one leaves the menu open, as a quick toggle never
//! closes its menu.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::gnome::{A11ySettings, A11yToggle};
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{
    self, style, Align, BakeCache, Dir, Painter, QuickToggle, ShapedText, TextShaper, TextStyle,
};

/// `.quick-settings` padding, `$base_padding * 3` (`_quick-settings.scss:2`).
const PAD: f64 = 18.;
/// `.quick-settings` radius, `$modal_radius * 2.25` (`_quick-settings.scss:3`).
const RADIUS: f64 = 36.;
/// `N_A11Y_MENU_COLUMNS` (`loginDialog.js:51`).
const COLS: usize = 2;

/// Each toggle's glyph (`accessibility.js:154-318`).
fn icon(toggle: A11yToggle) -> &'static str {
    match toggle {
        A11yToggle::HighContrast => "accessibility-high-contrast-symbolic",
        A11yToggle::Zoom => "accessibility-zoom-symbolic",
        A11yToggle::LargeText => "accessibility-large-text-symbolic",
        A11yToggle::ScreenReader => "accessibility-screen-reader-symbolic",
        A11yToggle::ScreenKeyboard => "input-keyboard-symbolic",
        A11yToggle::VisualAlerts => "accessibility-visual-alerts-symbolic",
        A11yToggle::StickyKeys => "accessibility-sticky-keys-symbolic",
        A11yToggle::SlowKeys => "accessibility-slow-keys-symbolic",
        A11yToggle::BounceKeys => "accessibility-bounce-keys-symbolic",
        A11yToggle::MouseKeys => "accessibility-mouse-keys-symbolic",
    }
}

/// Tile `i`'s box, menu-local.
fn tile_rect(i: usize) -> Rectangle<f64, Logical> {
    let (row, col) = ((i / COLS) as f64, (i % COLS) as f64);
    Rectangle::new(
        Point::from((
            PAD + col * (QuickToggle::width() + QuickToggle::GAP),
            PAD + row * (QuickToggle::HEIGHT + QuickToggle::GAP),
        )),
        Size::from((QuickToggle::width(), QuickToggle::HEIGHT)),
    )
}

pub struct LoginA11yMenu {
    settings: A11ySettings,
    hovered: Option<usize>,
    focused: Option<usize>,
    revision: u64,
    accent: widget::Rgba,
    cache: RefCell<BakeCache>,
}

impl LoginA11yMenu {
    pub fn new(settings: A11ySettings, accent: [u8; 3]) -> Self {
        Self {
            settings,
            hovered: None,
            focused: None,
            revision: 0,
            accent: style::accent_rgba(accent),
            cache: RefCell::new(BakeCache::new()),
        }
    }

    /// The toggles are `settings.bind`-ed (`accessibility.js:157-160`): a write from anywhere moves
    /// them. Returns whether anything changed.
    pub fn set_settings(&mut self, settings: A11ySettings) -> bool {
        if settings == self.settings {
            return false;
        }
        self.settings = settings;
        self.revision += 1;
        true
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        let rows = A11yToggle::ALL.len().div_ceil(COLS) as f64;
        Size::from((
            2. * PAD + COLS as f64 * QuickToggle::width() + (COLS - 1) as f64 * QuickToggle::GAP,
            2. * PAD + rows * QuickToggle::HEIGHT + (rows - 1.) * QuickToggle::GAP,
        ))
    }

    pub fn corner_radius(&self) -> f64 {
        RADIUS
    }

    fn hit(pos: Point<f64, Logical>) -> Option<usize> {
        (0..A11yToggle::ALL.len()).find(|&i| tile_rect(i).contains(pos))
    }

    fn flip(&self, i: usize) -> PopoverAction {
        let toggle = A11yToggle::ALL[i];
        PopoverAction::FlipA11yQuickToggle {
            toggle,
            on: !self.settings.get(toggle),
        }
    }

    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        match Self::hit(pos) {
            Some(i) => {
                if self.focused != Some(i) {
                    self.focused = Some(i);
                    self.revision += 1;
                }
                self.flip(i)
            }
            None => PopoverAction::Consumed,
        }
    }

    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let hovered = pos.and_then(Self::hit);
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        self.revision += 1;
        true
    }

    /// Arrows walk the grid; Tab-order is the construction order.
    pub fn nav(&mut self, dir: Dir) -> bool {
        let n = A11yToggle::ALL.len();
        let next = match (self.focused, dir) {
            (None, _) => Some(0),
            (Some(i), Dir::Down) => Some(i + COLS).filter(|&j| j < n),
            (Some(i), Dir::Up) => i.checked_sub(COLS),
            (Some(i), Dir::Right) => Some(i + 1).filter(|&j| j % COLS != 0 && j < n),
            (Some(i), Dir::Left) => (i % COLS != 0).then(|| i - 1),
            (Some(i), Dir::TabForward) => Some((i + 1) % n),
            (Some(i), Dir::TabBackward) => Some((i + n - 1) % n),
        };
        match next {
            Some(j) if Some(j) != self.focused => {
                self.focused = Some(j);
                self.revision += 1;
                true
            }
            _ => false,
        }
    }

    /// Enter or Space on a focused toggle flips it; the menu stays.
    pub fn activate_focused(&mut self) -> PopoverAction {
        match self.focused {
            Some(i) => self.flip(i),
            None => PopoverAction::Consumed,
        }
    }

    pub fn focused_label(&self) -> Option<&'static str> {
        self.focused.map(|i| A11yToggle::ALL[i].label())
    }

    /// Whether toggle `i` reads as checked. Test-only.
    #[cfg(test)]
    pub fn checked(&self, i: usize) -> bool {
        self.settings.get(A11yToggle::ALL[i])
    }

    /// Tile `i`'s centre, menu-local. Test-only.
    #[cfg(test)]
    pub fn tile_centre(i: usize) -> Point<f64, Logical> {
        let r = tile_rect(i);
        r.loc + Point::from((r.size.w / 2., r.size.h / 2.))
    }

    fn tile(&self, i: usize) -> QuickToggle {
        QuickToggle {
            rect: tile_rect(i),
            checked: self.settings.get(A11yToggle::ALL[i]),
            hovered: self.hovered == Some(i),
            focused: self.focused == Some(i),
        }
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();
        // Glyphs first: they sit over their tiles.
        for i in 0..A11yToggle::ALL.len() {
            let t = self.tile(i);
            if let Some(el) = widget::icon_element(
                renderer,
                icons,
                &[icon(A11yToggle::ALL[i])],
                QuickToggle::ICON_PX,
                scale,
                style::TEXT,
                origin,
                t.icon_centre(),
            ) {
                elements.push(el);
            }
        }
        let baked = widget::bake(
            renderer,
            &mut self.cache.borrow_mut(),
            scale,
            self.logical_size(),
            self.revision,
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                A11yToggle::ALL
                    .iter()
                    .map(|t| shaper.shape(t.label(), TextStyle::new(QuickToggle::TITLE_PT).bold()))
                    .collect::<anyhow::Result<Vec<ShapedText>>>()
            },
            |frame, phys, runs| {
                let mut p = Painter::new(frame, scale, phys);
                // The shared popover chrome draws the box behind this.
                p.clear(style::TRANSPARENT)?;
                for (i, run) in runs.iter().enumerate() {
                    let t = self.tile(i);
                    p.quick_toggle(&t, self.accent)?;
                    p.text_clipped(
                        run,
                        t.title_origin(),
                        Align::LEFT_MIDDLE,
                        style::TEXT,
                        t.rect,
                    )?;
                }
                Ok(())
            },
        );
        match baked {
            Ok(buffer) => elements.push(TextureRenderElement::from_texture_buffer(
                buffer,
                origin,
                1.,
                None,
                None,
                Kind::Unspecified,
            )),
            Err(err) => tracing::error!("error drawing the login accessibility menu: {err:#}"),
        }
        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two columns of five, in construction order, spaced as `.quick-settings-grid`.
    #[test]
    fn ten_toggles_in_two_columns() {
        let m = LoginA11yMenu::new(A11ySettings::default(), [0x35, 0x84, 0xe4]);
        let w = QuickToggle::width();
        assert_eq!(tile_rect(0).loc, Point::from((PAD, PAD)));
        assert_eq!(tile_rect(1).loc.x, PAD + w + 12.);
        assert_eq!(tile_rect(9).loc.y, PAD + 4. * (48. + 12.));
        let size = m.logical_size();
        assert_eq!(size.w, 2. * PAD + 2. * w + 12.);
        assert_eq!(size.h, 2. * PAD + 5. * 48. + 4. * 12.);
    }

    /// A click flips the toggle under it and asks for the opposite of what it shows.
    #[test]
    fn a_click_flips_its_toggle() {
        let mut settings = A11ySettings::default();
        settings.set(A11yToggle::Zoom, true);
        let mut m = LoginA11yMenu::new(settings, [0x35, 0x84, 0xe4]);
        assert_eq!(
            m.pointer_click(LoginA11yMenu::tile_centre(1)),
            PopoverAction::FlipA11yQuickToggle {
                toggle: A11yToggle::Zoom,
                on: false,
            }
        );
        assert_eq!(m.focused_label(), Some("Zoom"));
        assert_eq!(
            m.pointer_click(Point::from((1., 1.))),
            PopoverAction::Consumed
        );
    }

    #[test]
    fn arrows_walk_the_grid() {
        let mut m = LoginA11yMenu::new(A11ySettings::default(), [0x35, 0x84, 0xe4]);
        assert!(m.nav(Dir::Down));
        assert_eq!(m.focused_label(), Some("High Contrast"));
        assert!(m.nav(Dir::Right));
        assert_eq!(m.focused_label(), Some("Zoom"));
        assert!(!m.nav(Dir::Right), "the right column is the last");
        assert!(m.nav(Dir::Down));
        assert_eq!(m.focused_label(), Some("Screen Reader"));
        assert!(m.nav(Dir::Left));
        assert_eq!(m.focused_label(), Some("Large Text"));
    }
}
