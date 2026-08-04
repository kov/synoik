//! The panel input-source (keyboard-layout) popover menu — the fork's port of
//! gnome-shell's `InputSourceIndicator` popup (`js/ui/status/keyboard.js`
//! `InputSourceIndicator._init` / `_addSourceIndicators`).
//!
//! A simple vertical menu opened from the panel `keyboard` role: one row per
//! configured layout (its display name, with the active one marked and its short
//! label on the right), then a separator and the two fixed trailing items
//! gnome-shell always appends — "Show Keyboard Layout" (activates GNOME Tecla)
//! and "Keyboard Settings" (opens the control-center keyboard panel). Clicking a
//! layout row switches the active group and records it in `mru-sources`.
//!
//! Divergences from gnome-shell: the active row is marked with a check
//! (`object-select-symbolic`) rather than a radio `Ornament.DOT`; IBus property
//! sub-menus are absent (no IBus in this fork).

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{
    self, style, Align, BakeCache, Painter, ShapedText, TextShaper, TextStyle,
};

const PAD: f64 = 8.;
const ROW_H: f64 = 32.;
const ROW_GAP: f64 = 2.;
/// Left/right margin of the hover band inside the card.
const ROW_MARGIN: f64 = 6.;
/// Text inset from the row's left edge (inside the ornament column for a layout row).
const TEXT_INSET: f64 = 12.;
/// The leading ornament column (the active row's check sits here); reserved on
/// every layout row so the display name doesn't shift when the mark appears.
const ORN_W: f64 = 22.;
const ICON: f64 = 16.;
/// Gap between the display name and the right-aligned short label.
const LABEL_GAP: f64 = 16.;
/// Extra space above the first trailing item (the separator break).
const SEP_EXTRA: f64 = 9.;
const RADIUS: f64 = 14.;
const MIN_W: f64 = 200.;

/// The row separator rule. Local: the separator alpha diverges across widgets
/// (0.12 here vs 0.10/0.22 elsewhere) pending a reference reconciliation.
const SEPARATOR: [f32; 4] = [1., 1., 1., 0.12];

/// Row text size (`%heading`, GNOME points). `text_px()` is its logical px, used
/// only for GPU-free width measurement in [`InputSourceMenu::logical_size`]; the
/// bake shapes via [`widget::TextStyle`] so no draw site touches the px.
const TEXT_PT: f64 = 11.;
fn text_px() -> f64 {
    crate::ui::pt_to_px(TEXT_PT)
}

/// One configured layout as shown in the menu (gnome-shell's `LayoutMenuItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputSourceItem {
    /// The full display name (e.g. "English (US)"), from the live xkb layout name.
    pub display: String,
    /// The short label (e.g. "us"), matching the panel indicator.
    pub short: String,
}

/// Which menu row an index maps to.
enum RowKind {
    /// A configured layout at this source index.
    Layout(usize),
    ShowLayout,
    Settings,
}

/// The input-source popover menu content.
pub struct InputSourceMenu {
    items: Vec<InputSourceItem>,
    active: usize,
    hovered: Option<usize>,
    revision: u64,
    bg_cache: RefCell<BakeCache>,
}

impl InputSourceMenu {
    pub fn new(items: Vec<InputSourceItem>, active: usize) -> Self {
        Self {
            items,
            active,
            hovered: None,
            revision: 0,
            bg_cache: RefCell::new(BakeCache::new()),
        }
    }

    /// Total row count: one per layout, then the two fixed trailing items.
    fn row_count(&self) -> usize {
        self.items.len() + 2
    }

    fn row_kind(&self, k: usize) -> RowKind {
        let n = self.items.len();
        if k < n {
            RowKind::Layout(k)
        } else if k == n {
            RowKind::ShowLayout
        } else {
            RowKind::Settings
        }
    }

    /// The top y of row `k` (content space). The trailing items sit below a
    /// `SEP_EXTRA` break after the last layout row.
    fn row_top(&self, k: usize) -> f64 {
        let sep = if k >= self.items.len() { SEP_EXTRA } else { 0. };
        PAD + k as f64 * (ROW_H + ROW_GAP) + sep
    }

    /// Row `k`'s hover/click band (full width less the side margins).
    fn row_rect(&self, k: usize, width: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((ROW_MARGIN, self.row_top(k))),
            Size::from((width - 2. * ROW_MARGIN, ROW_H)),
        )
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        let measure =
            |s: &str| synoik_vk::text::measure_line_width_weighted(s, text_px() as f32, false);
        let w_display = self
            .items
            .iter()
            .map(|i| measure(&i.display))
            .fold(0., f64::max);
        let w_short = self
            .items
            .iter()
            .map(|i| measure(&i.short))
            .fold(0., f64::max);
        let w_action = measure("Show Keyboard Layout").max(measure("Keyboard Settings"));
        let content = (ORN_W + w_display + LABEL_GAP + w_short).max(w_action);
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

    /// Route a menu-local click to its action.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let width = self.logical_size().w;
        for k in 0..self.row_count() {
            if self.row_rect(k, width).contains(pos) {
                return match self.row_kind(k) {
                    RowKind::Layout(i) => PopoverAction::SetInputSource(i),
                    // gnome-shell activates `org.gnome.Tecla.desktop` (`_showLayout`).
                    RowKind::ShowLayout => {
                        PopoverAction::Spawn(vec!["gtk-launch".into(), "org.gnome.Tecla".into()])
                    }
                    // `addSettingsAction(_('Keyboard Settings'), 'gnome-keyboard-panel.desktop')`.
                    RowKind::Settings => {
                        PopoverAction::Spawn(vec!["gnome-control-center".into(), "keyboard".into()])
                    }
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

    /// Shape every row's text — the miss-only prepare phase for [`widget::bake`].
    /// The `TextShaper` performs the pt → logical → physical (`× scale`) sizing, so
    /// there is no manual scale multiply here. Returns one `(label, optional short)`
    /// run per row, in row order.
    #[allow(clippy::type_complexity)]
    fn shape_rows(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<Vec<(ShapedText, Option<ShapedText>)>> {
        let mut shaper = TextShaper::new(renderer, scale);
        let style = TextStyle::new(TEXT_PT);
        (0..self.row_count())
            .map(|k| {
                let (label, short) = match self.row_kind(k) {
                    RowKind::Layout(i) => (
                        self.items[i].display.clone(),
                        Some(self.items[i].short.clone()),
                    ),
                    RowKind::ShowLayout => ("Show Keyboard Layout".to_owned(), None),
                    RowKind::Settings => ("Keyboard Settings".to_owned(), None),
                };
                let label_run = shaper.shape(&label, style)?;
                let short_run = short.map(|s| shaper.shape(&s, style)).transpose()?;
                Ok((label_run, short_run))
            })
            .collect()
    }

    /// Paint the card chrome (background, hover wash, separator, all row text) via a
    /// logical-unit [`Painter`] — the paint phase for [`widget::bake`]. `phys` is the
    /// full buffer size; `row_runs` are the runs shaped in [`Self::shape_rows`]. No
    /// scale multiply appears here: the `Painter` owns it.
    fn paint_bg(
        &self,
        frame: &mut VulkanFrame,
        phys: Size<i32, Physical>,
        scale: f64,
        row_runs: &[(ShapedText, Option<ShapedText>)],
    ) -> anyhow::Result<()> {
        let size = self.logical_size();
        let mut p = Painter::new(frame, scale, phys);

        // Transparent bg: the shared popover chrome (`PanelPopover::render`) draws the
        // `.popup-menu-content` box fill behind this content.
        p.clear(style::TRANSPARENT)?;

        // The separator rule centered in the break above the trailing items.
        let sep_y = self.row_top(self.items.len()) - SEP_EXTRA / 2.;
        let sep = Rectangle::new(
            Point::from((TEXT_INSET, sep_y)),
            Size::from((size.w - 2. * TEXT_INSET, 1.)),
        );
        p.fill_rounded(sep, 0., SEPARATOR)?;

        for (k, (label_run, short_run)) in row_runs.iter().enumerate() {
            let rrect = self.row_rect(k, size.w);
            if self.hovered == Some(k) {
                p.fill_rounded(rrect, 8., style::HOVER_WASH)?;
            }
            // Layout rows reserve the ornament column; trailing items don't.
            let is_layout = matches!(self.row_kind(k), RowKind::Layout(_));
            let label_x = if is_layout {
                TEXT_INSET + ORN_W
            } else {
                TEXT_INSET
            };
            let cy = rrect.loc.y + rrect.size.h / 2.;
            p.text(
                label_run,
                Point::from((label_x, cy)),
                Align::LEFT_MIDDLE,
                style::TEXT,
            )?;

            // The short label, right-aligned and dimmed.
            if let Some(run) = short_run {
                let right = size.w - TEXT_INSET;
                p.text(
                    run,
                    Point::from((right, cy)),
                    Align::RIGHT_MIDDLE,
                    style::MUTED,
                )?;
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

    /// The menu's render elements at `origin`, topmost first: the active row's
    /// check icon, then the baked card chrome beneath it.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();

        // The active layout's check, centered in its ornament column.
        if self.active < self.items.len() {
            let rrect = self.row_rect(self.active, self.logical_size().w);
            let center = Point::from((
                rrect.loc.x + TEXT_INSET + ORN_W / 2. - ROW_MARGIN,
                rrect.loc.y + rrect.size.h / 2.,
            ));
            if let Some(el) = widget::icon_element(
                renderer,
                icons,
                style::CHECK_ICONS,
                ICON,
                scale,
                style::TEXT,
                origin,
                center,
            ) {
                elements.push(el);
            }
        }

        match self.bg_texture(renderer, scale) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
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
            Err(err) => tracing::error!("error drawing the input-source menu: {err:#}"),
        }
        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(n: usize, active: usize) -> InputSourceMenu {
        let items = (0..n)
            .map(|i| InputSourceItem {
                display: format!("Layout {i}"),
                short: format!("l{i}"),
            })
            .collect();
        InputSourceMenu::new(items, active)
    }

    #[test]
    fn rows_map_to_layout_then_fixed_items() {
        let mut m = menu(2, 0);
        let w = m.logical_size().w;
        let hit = |m: &mut InputSourceMenu, k: usize| {
            let r = m.row_rect(k, w);
            m.pointer_click(r.loc + Point::from((r.size.w / 2., r.size.h / 2.)))
        };
        assert_eq!(hit(&mut m, 0), PopoverAction::SetInputSource(0));
        assert_eq!(hit(&mut m, 1), PopoverAction::SetInputSource(1));
        // Row 2 = "Show Keyboard Layout", row 3 = "Keyboard Settings".
        assert!(matches!(hit(&mut m, 2), PopoverAction::Spawn(c) if c[0] == "gtk-launch"));
        assert!(
            matches!(hit(&mut m, 3), PopoverAction::Spawn(c) if c == ["gnome-control-center", "keyboard"])
        );
    }

    #[test]
    fn size_grows_with_layout_count() {
        assert!(menu(4, 0).logical_size().h > menu(2, 0).logical_size().h);
        // Always at least the two trailing rows.
        assert!(menu(0, 0).logical_size().h > 0.);
    }

    #[test]
    fn hover_tracks_rows_and_reports_change() {
        let mut m = menu(2, 0);
        let w = m.logical_size().w;
        let r0 = m.row_rect(0, w);
        assert!(m.pointer_hover(Some(r0.loc + Point::from((4., r0.size.h / 2.)))));
        assert_eq!(m.hovered, Some(0));
        // Same row again: no change.
        assert!(!m.pointer_hover(Some(r0.loc + Point::from((6., r0.size.h / 2.)))));
        // Leaving clears it.
        assert!(m.pointer_hover(None));
        assert_eq!(m.hovered, None);
    }

    /// The regression pin for the minuscule-text bug (`3c7473be`): baked at scales
    /// {1, 1.5, 2} the buffer must be physically scaled AND the glyph ink must grow
    /// with the scale, not shrink. Shaping the text at logical px (the bug) would
    /// render it ~½-height at scale 2 and trip the harness. Skips with no device.
    #[test]
    fn popover_text_scales_with_output() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping popover_text_scales_with_output: no Vulkan device ({e})");
                return;
            }
        };
        let m = menu(2, 0);
        crate::ui::widget::assert_scale_correct(&mut vk, m.logical_size(), |vk, scale| {
            m.bg_texture(vk, scale).expect("bake input-source popover")
        });
    }
}
