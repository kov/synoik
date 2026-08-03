//! The panel accessibility popover menu — the fork's port of gnome-shell's
//! `ATIndicator` menu (`js/ui/status/accessibility.js:45-86`).
//!
//! Ten `PopupSwitchMenuItem` rows in gnome-shell's construction order (High Contrast,
//! Zoom, Large Text, Screen Reader, Screen Keyboard, Visual Alerts, Sticky/Slow/Bounce/
//! Mouse Keys), then a separator and the "Accessibility Settings" action. Each row is a
//! label with a [`widget::Switch`] at its right end; clicking one flips the backing
//! gsettings key **and closes the menu**, which is what `PopupSwitchMenuItem.activate`
//! does for a pointer event (`js/ui/popupMenu.js:539-550` — only Space returns early
//! and keeps the menu open).
//!
//! The menu is built from an [`A11ySettings`] snapshot, but it is not frozen: GNOME's
//! rows are `settings.bind`-ed (`accessibility.js:110`), so a key written by anyone else
//! while the menu is up moves the switch under it. [`A11yMenu::set_settings`] is that
//! path, fed from the gsettings watcher.
//!
//! Divergences: the switch snaps between positions instead of sliding (it *is* seen —
//! the clicked row flips while the menu fades out — but a 100ms travel
//! (`_switches.scss:9`) inside a 150ms fade is a refinement, not the behavior); and the
//! Large Text row is not made insensitive when `text-scaling-factor` is locked down
//! (`accessibility.js:141-143`) — dconf lockdown isn't modeled anywhere in the fork.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::gnome::{A11ySettings, A11yToggle};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{
    self, style, Align, BakeCache, Painter, ShapedText, Switch, TextShaper, TextStyle,
};

const PAD: f64 = 8.;
/// Row height: the switch is the tallest thing in a row, so it sets the floor
/// (`Switch::HEIGHT` 26 plus the menu item's vertical padding).
const ROW_H: f64 = 36.;
const ROW_GAP: f64 = 2.;
/// Left/right margin of the hover band inside the card.
const ROW_MARGIN: f64 = 6.;
/// Text inset from the row's left edge.
const TEXT_INSET: f64 = 12.;
/// Extra space above the trailing settings item (the separator break).
const SEP_EXTRA: f64 = 9.;
const RADIUS: f64 = 14.;
const MIN_W: f64 = 260.;

/// The row separator rule, matching the input-source menu's local value.
const SEPARATOR: [f32; 4] = [1., 1., 1., 0.12];

/// The trailing action row (`addSettingsAction(_('Accessibility Settings'),
/// 'gnome-universal-access-panel.desktop')`, `accessibility.js:84-85`).
const SETTINGS_LABEL: &str = "Accessibility Settings";

/// Row text size (`%heading`, GNOME points).
const TEXT_PT: f64 = 11.;
fn text_px() -> f64 {
    crate::ui::pt_to_px(TEXT_PT)
}

/// Which menu row an index maps to.
enum RowKind {
    /// One of the ten switch rows, in [`A11yToggle::ALL`] order.
    Toggle(A11yToggle),
    Settings,
}

/// The accessibility popover menu content.
pub struct A11yMenu {
    settings: A11ySettings,
    hovered: Option<usize>,
    revision: u64,
    accent: [f32; 4],
    bg_cache: RefCell<BakeCache>,
}

impl A11yMenu {
    /// `accent` is straight RGB (e.g. `gnome_settings.accent_color`) — the switch's
    /// on-state track fill.
    pub fn new(settings: A11ySettings, accent: [u8; 3]) -> Self {
        Self {
            settings,
            hovered: None,
            revision: 0,
            accent: widget::style::accent_rgba(accent),
            bg_cache: RefCell::new(BakeCache::new()),
        }
    }

    /// Adopt fresh state while the menu is open — GNOME's rows are `settings.bind`-ed
    /// (`accessibility.js:110`), so an external write moves the switch under an open
    /// menu. Returns whether it changed, so the caller can queue a redraw; the bake is
    /// keyed on `revision`, so it must move with the state.
    pub fn set_settings(&mut self, settings: A11ySettings) -> bool {
        if settings == self.settings {
            return false;
        }
        self.settings = settings;
        self.revision += 1;
        true
    }

    /// Ten switch rows plus the trailing settings item.
    fn row_count(&self) -> usize {
        A11yToggle::ALL.len() + 1
    }

    fn row_kind(&self, k: usize) -> RowKind {
        match A11yToggle::ALL.get(k) {
            Some(&toggle) => RowKind::Toggle(toggle),
            None => RowKind::Settings,
        }
    }

    /// The top y of row `k` (content space). The settings item sits below a
    /// `SEP_EXTRA` break after the last switch row.
    fn row_top(&self, k: usize) -> f64 {
        let sep = if k >= A11yToggle::ALL.len() {
            SEP_EXTRA
        } else {
            0.
        };
        PAD + k as f64 * (ROW_H + ROW_GAP) + sep
    }

    /// Row `k`'s hover/click band (full width less the side margins).
    fn row_rect(&self, k: usize, width: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((ROW_MARGIN, self.row_top(k))),
            Size::from((width - 2. * ROW_MARGIN, ROW_H)),
        )
    }

    /// The switch rect inside a switch row: right-aligned, vertically centered.
    fn switch_rect(&self, k: usize, width: f64) -> Rectangle<f64, Logical> {
        let row = self.row_rect(k, width);
        let size = Switch::size();
        Rectangle::new(
            Point::from((
                row.loc.x + row.size.w - TEXT_INSET + ROW_MARGIN - size.w,
                row.loc.y + (row.size.h - size.h) / 2.,
            )),
            size,
        )
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        let measure =
            |s: &str| niri_vk::text::measure_line_width_weighted(s, text_px() as f32, false);
        let w_label = A11yToggle::ALL
            .iter()
            .map(|t| measure(t.label()))
            .fold(0., f64::max);
        // A switch row is label + gap + switch; the settings row is just its label.
        let content = (w_label + TEXT_INSET + Switch::WIDTH).max(measure(SETTINGS_LABEL));
        let width = (TEXT_INSET + content + TEXT_INSET).max(MIN_W);

        let total = self.row_count();
        let height = 2. * PAD
            + total as f64 * ROW_H
            + (total.saturating_sub(1)) as f64 * ROW_GAP
            + SEP_EXTRA;
        Size::from((width, height))
    }

    /// The menu box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        RADIUS
    }

    /// Whether row `k`'s switch reads as on, or `None` for the settings row. Test-only.
    #[cfg(test)]
    pub fn row_state(&self, k: usize) -> Option<bool> {
        match self.row_kind(k) {
            RowKind::Toggle(toggle) => Some(self.settings.get(toggle)),
            RowKind::Settings => None,
        }
    }

    /// Row `k`'s center in menu-local logical coordinates, so a test can click a row
    /// without re-deriving the padding and row height (which it would then silently get
    /// wrong when they change).
    #[cfg(test)]
    pub fn row_center(&self, k: usize) -> Point<f64, Logical> {
        let r = self.row_rect(k, self.logical_size().w);
        r.loc + Point::from((r.size.w / 2., r.size.h / 2.))
    }

    /// Route a menu-local click to its action. The whole row is the target (GNOME's
    /// `PopupSwitchMenuItem` toggles from anywhere in the item, not just the switch).
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let width = self.logical_size().w;
        for k in 0..self.row_count() {
            if self.row_rect(k, width).contains(pos) {
                return match self.row_kind(k) {
                    RowKind::Toggle(toggle) => PopoverAction::SetA11yToggle {
                        toggle,
                        on: !self.settings.get(toggle),
                    },
                    RowKind::Settings => PopoverAction::Spawn(vec![
                        "gtk-launch".into(),
                        "gnome-universal-access-panel".into(),
                    ]),
                };
            }
        }
        PopoverAction::Consumed
    }

    /// Update the hovered row from a menu-local pointer position (`None` clears).
    /// Returns whether it changed (so the caller can redraw).
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let width = self.logical_size().w;
        let hovered =
            pos.and_then(|p| (0..self.row_count()).find(|&k| self.row_rect(k, width).contains(p)));
        if hovered != self.hovered {
            self.hovered = hovered;
            self.revision += 1;
            true
        } else {
            false
        }
    }

    /// Shape every row's label — the miss-only prepare phase for [`widget::bake`].
    fn shape_rows(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<Vec<ShapedText>> {
        let mut shaper = TextShaper::new(renderer, scale);
        let style = TextStyle::new(TEXT_PT);
        (0..self.row_count())
            .map(|k| {
                let label = match self.row_kind(k) {
                    RowKind::Toggle(toggle) => toggle.label(),
                    RowKind::Settings => SETTINGS_LABEL,
                };
                shaper.shape(label, style)
            })
            .collect()
    }

    /// Paint the card chrome (hover wash, separator, labels, switches) via a
    /// logical-unit [`Painter`] — the paint phase for [`widget::bake`].
    fn paint_bg(
        &self,
        frame: &mut VulkanFrame,
        phys: Size<i32, Physical>,
        scale: f64,
        row_runs: &[ShapedText],
    ) -> anyhow::Result<()> {
        let size = self.logical_size();
        let mut p = Painter::new(frame, scale, phys);

        // Transparent bg: the shared popover chrome (`PanelPopover::render`) draws the
        // `.popup-menu-content` box fill behind this content.
        p.clear(style::TRANSPARENT)?;

        // The separator rule centered in the break above the settings item.
        let sep_y = self.row_top(A11yToggle::ALL.len()) - SEP_EXTRA / 2.;
        let sep = Rectangle::new(
            Point::from((TEXT_INSET, sep_y)),
            Size::from((size.w - 2. * TEXT_INSET, 1.)),
        );
        p.fill_rounded(sep, 0., SEPARATOR)?;

        for (k, run) in row_runs.iter().enumerate() {
            let rrect = self.row_rect(k, size.w);
            let hovered = self.hovered == Some(k);
            if hovered {
                p.fill_rounded(rrect, 8., style::HOVER_WASH)?;
            }
            let cy = rrect.loc.y + rrect.size.h / 2.;
            p.text(
                run,
                Point::from((TEXT_INSET, cy)),
                Align::LEFT_MIDDLE,
                style::TEXT,
            )?;

            if let RowKind::Toggle(toggle) = self.row_kind(k) {
                let on = self.settings.get(toggle);
                p.toggle_switch(self.switch_rect(k, size.w), on, hovered, self.accent)?;
            }
        }
        Ok(())
    }

    fn bg_texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        widget::bake(
            renderer,
            &mut self.bg_cache.borrow_mut(),
            scale,
            self.logical_size(),
            self.revision,
            |renderer| self.shape_rows(renderer, scale),
            |frame, phys, row_runs| self.paint_bg(frame, phys, scale, row_runs),
        )
    }

    /// The menu's render element at `origin` — the whole menu is one bake (unlike the
    /// input-source menu, no row needs an icon composited on top).
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        match self.bg_texture(renderer, scale) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                vec![TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                )]
            }
            Err(err) => {
                tracing::error!("error drawing the accessibility menu: {err:#}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(settings: A11ySettings) -> A11yMenu {
        A11yMenu::new(settings, [0x35, 0x84, 0xe4])
    }

    fn click_row(m: &mut A11yMenu, k: usize) -> PopoverAction {
        let w = m.logical_size().w;
        let r = m.row_rect(k, w);
        m.pointer_click(r.loc + Point::from((r.size.w / 2., r.size.h / 2.)))
    }

    /// The row order is gnome-shell's construction order (`accessibility.js:45-85`),
    /// and each switch row asks for the *opposite* of its current state.
    #[test]
    fn rows_follow_gnome_order_and_toggle_the_right_key() {
        let mut m = menu(A11ySettings::default());
        for (k, &toggle) in A11yToggle::ALL.iter().enumerate() {
            assert_eq!(
                click_row(&mut m, k),
                PopoverAction::SetA11yToggle { toggle, on: true },
                "row {k} should be {toggle:?}",
            );
        }
        // The trailing item is the settings action, past the separator.
        assert!(matches!(click_row(&mut m, A11yToggle::ALL.len()),
                PopoverAction::Spawn(c) if c == ["gtk-launch", "gnome-universal-access-panel"]));
    }

    /// An already-on row asks to turn itself off — the click is a flip, not a set.
    #[test]
    fn clicking_an_on_row_turns_it_off() {
        let mut settings = A11ySettings::default();
        settings.set(A11yToggle::Zoom, true);
        settings.set(A11yToggle::LargeText, true);
        let mut m = menu(settings);

        assert_eq!(
            click_row(&mut m, 1),
            PopoverAction::SetA11yToggle {
                toggle: A11yToggle::Zoom,
                on: false
            }
        );
        assert_eq!(
            click_row(&mut m, 2),
            PopoverAction::SetA11yToggle {
                toggle: A11yToggle::LargeText,
                on: false
            }
        );
    }

    /// A pointer click on a switch row closes the menu — `PopupSwitchMenuItem.activate`
    /// toggles and then falls through to `super.activate` for anything but Space
    /// (`js/ui/popupMenu.js:539-550`).
    #[test]
    fn toggling_closes_the_menu() {
        let mut m = menu(A11ySettings::default());
        assert!(click_row(&mut m, 0).closes_menu());
    }

    /// The switch sits inside its row, right-aligned, and never overlaps the label
    /// inset.
    #[test]
    fn switches_sit_at_the_right_end_of_their_row() {
        let m = menu(A11ySettings::default());
        let w = m.logical_size().w;
        for k in 0..A11yToggle::ALL.len() {
            let row = m.row_rect(k, w);
            let sw = m.switch_rect(k, w);
            assert!(row.contains_rect(sw), "switch {k} escapes its row");
            assert!(sw.loc.x > TEXT_INSET * 2., "switch {k} runs into the label");
        }
    }

    #[test]
    fn hover_tracks_rows_and_reports_change() {
        let mut m = menu(A11ySettings::default());
        let w = m.logical_size().w;
        let r0 = m.row_rect(0, w);
        assert!(m.pointer_hover(Some(r0.loc + Point::from((4., r0.size.h / 2.)))));
        assert_eq!(m.hovered, Some(0));
        assert!(!m.pointer_hover(Some(r0.loc + Point::from((6., r0.size.h / 2.)))));
        assert!(m.pointer_hover(None));
        assert_eq!(m.hovered, None);
    }

    /// An external write moves the switch under an open menu, and re-bakes: GNOME's rows
    /// are `settings.bind`-ed (`accessibility.js:110`), so the menu is not a frozen
    /// snapshot. The revision must move with the state or the cached bake would keep
    /// showing the old switch positions.
    #[test]
    fn an_external_write_moves_an_open_menus_switch() {
        let mut m = menu(A11ySettings::default());
        let before = m.revision;

        let mut changed = A11ySettings::default();
        changed.set(A11yToggle::Zoom, true);
        assert!(m.set_settings(changed));
        assert!(m.revision > before, "the bake must be invalidated");

        // The row now reads as on, so clicking it asks to turn it OFF.
        assert_eq!(
            click_row(&mut m, 1),
            PopoverAction::SetA11yToggle {
                toggle: A11yToggle::Zoom,
                on: false
            }
        );

        // Re-pushing the same state is a no-op (no redraw churn).
        assert!(!m.set_settings(changed));
    }

    /// The regression pin for the minuscule-text bug (`3c7473be`), as on every other
    /// popover: the bake must be physically scaled AND the glyph ink must grow with
    /// the scale. Skips with no device.
    #[test]
    fn popover_text_scales_with_output() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping popover_text_scales_with_output: no Vulkan device ({e})");
                return;
            }
        };
        let m = menu(A11ySettings::default());
        crate::ui::widget::assert_scale_correct(&mut vk, m.logical_size(), |vk, scale| {
            m.bg_texture(vk, scale).expect("bake a11y popover")
        });
    }
}
