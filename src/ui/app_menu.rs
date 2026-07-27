//! The app-icon context menu — gnome-shell's `AppMenu` (`js/ui/appMenu.js`), the
//! menu a right-click on a dash / app-grid / search-result icon pops up
//! (`AppIcon.popupMenu`, `appDisplay.js:3027-3052`).
//!
//! This is also the fork's first real `.popup-menu-item` surface: the metrics here
//! (`ITEM_PAD_*`, `ITEM_RADIUS`, the separator rule) are the shared popup-menu box
//! model, not app-menu specific, and the next menu should lift them out rather than
//! restate them.
//!
//! **Scope.** gnome-shell's menu has, in order: an "Open Windows" section, "New
//! Window", the `.desktop` action section, a discrete-GPU launch item, the favourite
//! toggle, "App Details", and "Quit". We build the middle three:
//!
//! - **New Window** — shown only when the app has no `new-window` desktop action, since that action
//!   would be a duplicate of it (`_updateNewWindowItem`, `appMenu.js:140-144`).
//! - **The action section** — one row per `.desktop` action, labelled with its display name
//!   (`setApp`, `appMenu.js:229-242`).
//! - **Pin to Dash / Unpin** — the favourite toggle (`_updateFavoriteItem`, `appMenu.js:146-162`).
//!
//! Deferred, each because it needs a capability the fork does not have yet:
//! **Open Windows** and **Quit** need per-window identity for a running app (we track
//! a window *count*, not a list — see `AppSystem::running`); **App Details** needs an
//! `org.gtk.Actions` call into `org.gnome.Software`; the **GPU** item needs
//! `switcheroo-control`, which this hardware does not have.
//!
//! Separators are inserted between *populated* groups only, which is what
//! gnome-shell's `_updateSeparatorVisibility` (`popupMenu.js`) arrives at by hiding
//! the ones that end up adjacent to nothing.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::app_system::AppEntry;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::popover::PopoverAction;
use crate::ui::widget::{
    self, style, Align, BakeCache, Painter, ShapedText, TextShaper, TextStyle,
};

/// `.popup-menu-content` padding — `$base_padding` (`_popovers.scss:28`).
const CONTENT_PAD: f64 = 6.;
/// `.popup-menu-item` vertical padding — `$base_padding * 1.5` (`%menuitem`,
/// `_common.scss:135`).
const ITEM_PAD_V: f64 = 9.;
/// `.popup-menu-item` horizontal padding — `$base_padding * 2`.
const ITEM_PAD_H: f64 = 12.;
/// An item's line box: the label, or the `$scalable_icon_size` an item's icon would
/// occupy — whichever is taller, and they are the same at this font size.
const ITEM_LINE_H: f64 = 16.;
/// A menu row's height, from the box model above.
const ROW_H: f64 = 2. * ITEM_PAD_V + ITEM_LINE_H; // 34
/// `$menuitem_border_radius = $base_border_radius * 1.5` (`_popovers.scss:6,39`).
const ITEM_RADIUS: f64 = 12.;
/// A separator row: the same item padding around a 1px rule
/// (`PopupSeparatorMenuItem` is a `popup-menu-item`, `popupMenu.js:300-306`;
/// `.popup-separator-menu-item-separator { height: 1px }`, `_popovers.scss:117-120`).
const SEP_H: f64 = 2. * ITEM_PAD_V + 1.;
/// The separator rule — `$borders_color`, white at 10% (`_popovers.scss:119`).
const SEPARATOR: [f32; 4] = [1., 1., 1., 0.1];
/// `.popup-menu-content` `border-radius: $modal_radius * 1.25` (`_popovers.scss:30`).
const RADIUS: f64 = 20.;

/// Row text: `.popup-menu` inherits the 11pt body size (`_popovers.scss:16-24`).
const TEXT_PT: f64 = 11.;
const TEXT_PX: f64 = crate::ui::pt_to_px(TEXT_PT);

/// `.popup-menu { min-width: 15em }` (`_popovers.scss:17`), em against the row font.
const MIN_W: f64 = 15. * TEXT_PX;
/// `.app-menu { max-width: 27.25em }` (`_popovers.scss:143-144`). A label wider than
/// this is ellipsized rather than widening the menu.
const MAX_W: f64 = 27.25 * TEXT_PX;

/// What activating a row does. Held by the row so the click handler stays a lookup;
/// the app id rides along so the input layer needs no state of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowAction {
    NewWindow,
    LaunchAction(String),
    ToggleFavorite,
}

/// One built row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Item { label: String, action: RowAction },
    Separator,
}

/// The app context menu's content.
pub struct AppMenu {
    /// The app this menu is for — every action carries it back out.
    app_id: String,
    rows: Vec<Row>,
    hovered: Option<usize>,
    revision: u64,
    bg_cache: RefCell<BakeCache>,
}

impl AppMenu {
    /// Build the menu for `entry`. `is_favorite` picks the toggle's label; it is read
    /// once at open time because activating any item closes the menu.
    pub fn new(entry: &AppEntry, is_favorite: bool) -> Self {
        // Group 1: the launch verbs. "New Window" is suppressed when a `new-window`
        // action would restate it (`appMenu.js:140-144`).
        let mut launch = Vec::new();
        if !entry.actions.iter().any(|a| a.id == "new-window") {
            launch.push(Row::Item {
                label: "New Window".to_owned(),
                action: RowAction::NewWindow,
            });
        }
        launch.extend(entry.actions.iter().map(|a| Row::Item {
            label: a.name.clone(),
            action: RowAction::LaunchAction(a.id.clone()),
        }));

        // Group 2: the favourite toggle.
        let favorite = vec![Row::Item {
            label: if is_favorite { "Unpin" } else { "Pin to Dash" }.to_owned(),
            action: RowAction::ToggleFavorite,
        }];

        let mut rows = Vec::new();
        for group in [launch, favorite] {
            if group.is_empty() {
                continue;
            }
            if !rows.is_empty() {
                rows.push(Row::Separator);
            }
            rows.extend(group);
        }

        Self {
            app_id: entry.id.clone(),
            rows,
            hovered: None,
            revision: 0,
            bg_cache: RefCell::new(BakeCache::new()),
        }
    }

    /// The top y of row `k` (content space).
    fn row_top(&self, k: usize) -> f64 {
        CONTENT_PAD
            + self.rows[..k]
                .iter()
                .map(|r| match r {
                    Row::Item { .. } => ROW_H,
                    Row::Separator => SEP_H,
                })
                .sum::<f64>()
    }

    /// Row `k`'s hover/click band. Items span the content box; a separator has no
    /// band worth hitting but keeps the same rect so indexing stays uniform.
    fn row_rect(&self, k: usize, width: f64) -> Rectangle<f64, Logical> {
        let h = match self.rows[k] {
            Row::Item { .. } => ROW_H,
            Row::Separator => SEP_H,
        };
        Rectangle::new(
            Point::from((CONTENT_PAD, self.row_top(k))),
            Size::from((width - 2. * CONTENT_PAD, h)),
        )
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        let widest = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Item { label, .. } => Some(niri_vk::text::measure_line_width_weighted(
                    label,
                    TEXT_PX as f32,
                    false,
                )),
                Row::Separator => None,
            })
            .fold(0., f64::max);
        let width = (2. * (CONTENT_PAD + ITEM_PAD_H) + widest).clamp(MIN_W, MAX_W);
        // `row_top(len)` is the content pad plus every row, so the trailing pad closes it.
        Size::from((width, self.row_top(self.rows.len()) + CONTENT_PAD))
    }

    /// The menu box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        RADIUS
    }

    /// Every item's label, top to bottom, separators excluded — the menu's contents
    /// as a person reads them. For the corpus.
    pub fn labels(&self) -> Vec<&str> {
        self.rows
            .iter()
            .filter_map(|r| match r {
                Row::Item { label, .. } => Some(label.as_str()),
                Row::Separator => None,
            })
            .collect()
    }

    /// The menu-local centre of the row labelled `label`, so the corpus can click a
    /// row by name rather than by arithmetic that would drift with the box model.
    pub fn row_center(&self, label: &str) -> Option<Point<f64, Logical>> {
        let width = self.logical_size().w;
        let k = self
            .rows
            .iter()
            .position(|r| matches!(r, Row::Item { label: l, .. } if l == label))?;
        let rect = self.row_rect(k, width);
        Some(Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + rect.size.h / 2.,
        )))
    }

    /// Route a menu-local click to its action. A click on a separator or on the
    /// content padding is consumed without doing anything, like gnome-shell's
    /// non-reactive separator items.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let width = self.logical_size().w;
        for k in 0..self.rows.len() {
            let Row::Item { action, .. } = &self.rows[k] else {
                continue;
            };
            if self.row_rect(k, width).contains(pos) {
                let id = self.app_id.clone();
                return match action {
                    RowAction::NewWindow => PopoverAction::AppNewWindow(id),
                    RowAction::LaunchAction(a) => PopoverAction::AppLaunchAction {
                        id,
                        action: a.clone(),
                    },
                    RowAction::ToggleFavorite => PopoverAction::AppToggleFavorite(id),
                };
            }
        }
        PopoverAction::Consumed
    }

    /// Update the hovered row (`None` clears). Returns whether it changed.
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let width = self.logical_size().w;
        let hovered = pos.and_then(|p| {
            (0..self.rows.len()).find(|&k| {
                matches!(self.rows[k], Row::Item { .. }) && self.row_rect(k, width).contains(p)
            })
        });
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        self.revision += 1;
        true
    }

    /// Shape every item's label — the miss-only prepare phase for [`widget::bake`].
    /// One entry per row; separators shape nothing.
    fn shape_rows(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<Vec<Option<ShapedText>>> {
        let mut shaper = TextShaper::new(renderer, scale);
        let style = TextStyle::new(TEXT_PT);
        self.rows
            .iter()
            .map(|row| match row {
                Row::Item { label, .. } => shaper.shape(label, style).map(Some),
                Row::Separator => Ok(None),
            })
            .collect()
    }

    fn paint_bg(
        &self,
        frame: &mut VulkanFrame,
        phys: Size<i32, Physical>,
        scale: f64,
        runs: &[Option<ShapedText>],
    ) -> anyhow::Result<()> {
        let size = self.logical_size();
        let mut p = Painter::new(frame, scale, phys);

        // Transparent: the shared popover chrome draws the `.popup-menu-content` box.
        p.clear(style::TRANSPARENT)?;

        for (k, run) in runs.iter().enumerate() {
            let rect = self.row_rect(k, size.w);
            match run {
                // The rule sits centred in the separator row's band.
                None => {
                    let rule = Rectangle::new(
                        Point::from((rect.loc.x, rect.loc.y + (SEP_H - 1.) / 2.)),
                        Size::from((rect.size.w, 1.)),
                    );
                    p.fill_rounded(rule, 0., SEPARATOR)?;
                }
                Some(run) => {
                    if self.hovered == Some(k) {
                        p.fill_rounded(rect, ITEM_RADIUS, style::HOVER_WASH)?;
                    }
                    p.text(
                        run,
                        Point::from((rect.loc.x + ITEM_PAD_H, rect.loc.y + rect.size.h / 2.)),
                        Align::LEFT_MIDDLE,
                        style::TEXT,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// The menu's render elements at `origin` — one baked card.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let texture = widget::bake(
            renderer,
            &mut self.bg_cache.borrow_mut(),
            scale,
            self.logical_size(),
            self.revision,
            |renderer| self.shape_rows(renderer, scale),
            |frame, phys, runs| self.paint_bg(frame, phys, scale, runs),
        );
        match texture {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
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
                tracing::error!("error baking the app menu: {err:#}");
                Vec::new()
            }
        }
    }
}
