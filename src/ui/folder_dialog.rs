//! The app-folder dialog — gnome-shell's `AppFolderDialog` (`appDisplay.js:2463-2916`).
//!
//! Clicking a folder tile in the app grid opens this: a shade over the whole monitor with a
//! 720×720 panel floating on it, the folder's name across the top and the folder's apps in
//! their own 3×3 paginated grid below. Clicking an app inside launches it; clicking outside
//! the panel, or pressing Escape, pops it back down.
//!
//! The inner view is **the app grid widget itself** — GNOME builds it from the same class
//! (`FolderView extends BaseAppView`, `FolderGrid extends AppGrid`, `appDisplay.js:2066-2112`),
//! differing only in the page modes and how a page spends its slack, so
//! [`AppGrid::folder_view`] configures those two and everything else (hover wash, captions,
//! pagination, dots, navigation arrows, batched icon uploads) comes along for free.
//!
//! Divergences from gnome-shell, deliberate:
//! - **Read-only.** Folder *editing* is out of scope for this slice, so there is no rename entry
//!   and no edit button beside the name. GNOME centers its label by balancing the edit button with
//!   an equally-sized ghost actor (`_addFolderNameEntry`, `appDisplay.js:2531-2601`); with neither
//!   present the label is simply centered in the band, which is the same place.
//! - The panel is **clamped** to the space the container leaves it. GNOME allocates its natural
//!   720² and lets a short screen clip it (which is why the container pads the top by the panel
//!   height at all, `_app-grid.scss:53-56`); we would have no way to reach what fell off.
//! - GNOME constrains the dialog to the **primary** monitor (`MonitorConstraint({primary: true})`,
//!   `appDisplay.js:2478`). We draw the overview's chrome on every output already, and the dialog
//!   follows that same divergence.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::Clock;
use crate::app_system::AppIconRef;
use crate::niri_render_elements;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::app_grid::{AppGrid, AppGridEntry, PageArrow};
use crate::ui::panel::PANEL_HEIGHT;
use crate::ui::widget::{self, style, Align, Painter};

/// `$app_folder_size` (`_app-grid.scss:4,60-61`) — the panel is a fixed square.
const SIZE: f64 = 720.;
/// `border-radius: $modal_radius * 4` (`_app-grid.scss:63`), and `$modal_radius` is
/// `$base_border_radius * 2` = 16 (`_common.scss:33,40`), so 64 — four times the radius of an
/// ordinary popover, which is what makes the panel read as a rounded card rather than a window.
const RADIUS: f64 = 64.;
/// `padding: 0 1px` (`_app-grid.scss:71`) — the room the inset border occupies.
const PAD_X: f64 = 1.;
/// `box-shadow: inset 0 0 0 1px $system_borders_color` (`_app-grid.scss:72`).
const BORDER_W: f64 = 1.;
/// `.folder-name-container` `padding: $base_padding * 4 $base_padding * 6` with
/// `padding-bottom: 0` (`_app-grid.scss:75-78`).
const NAME_PAD_TOP: f64 = 24.;
const NAME_PAD_X: f64 = 36.;
/// The folder name is a `.folder-name-label`, i.e. `%title_1` — 20pt at weight 800
/// (`_app-grid.scss:79-82`, `_common.scss:246-249`).
const NAME_PT: f64 = 20.;
/// `DIALOG_SHADE_NORMAL` (`appDisplay.js:57`): black at alpha 204/255.
const SHADE: [f32; 4] = [0., 0., 0., 204. / 255.];

niri_render_elements! {
    FolderDialogRenderElement => {
        Texture = TextureRenderElement<VkTexture>,
        SolidColor = SolidColorRenderElement,
    }
}

/// What a point over an open dialog hit. Everything inside the panel consumes the click;
/// everything outside it pops the dialog down, which is the whole of GNOME's click gesture
/// (`may-recognize` tests the point against `_viewBox.allocation`, `appDisplay.js:2480-2487`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogHit {
    /// An app tile inside the folder, at index `.0`.
    App(usize),
    /// A page-indicator dot for page `.0`.
    Page(usize),
    /// A page navigation arrow.
    Arrow(PageArrow),
    /// Inside the panel but not on a control — consumes the click, does nothing.
    Inside,
    /// Outside the panel: pop down.
    Outside,
}

/// The open folder's geometry within one output view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DialogLayout {
    /// The `.app-folder-dialog` panel.
    pub panel: Rectangle<f64, Logical>,
    /// The band the name label is centered in.
    pub name_band: Rectangle<f64, Logical>,
    /// The folder view's box — everything below the name, which is what the inner
    /// [`AppGrid`] lays its page out in.
    pub grid_area: Rectangle<f64, Logical>,
}

/// Lay the dialog out over an output `view` (logical, output coordinates).
pub fn layout(view: Rectangle<f64, Logical>) -> DialogLayout {
    // `.app-folder-dialog-container` pads the top by the panel height, so the panel is
    // centered in the work area rather than the screen (`_app-grid.scss:53-56`).
    let avail_y = view.loc.y + PANEL_HEIGHT;
    let avail_h = (view.size.h - PANEL_HEIGHT).max(0.);

    let w = SIZE.min(view.size.w);
    let h = SIZE.min(avail_h);
    let panel = Rectangle::new(
        Point::from((
            (view.loc.x + (view.size.w - w) / 2.).round(),
            (avail_y + (avail_h - h) / 2.).round(),
        )),
        Size::from((w, h)),
    );

    let name_h = crate::ui::line_height_px(NAME_PT);
    let name_band = Rectangle::new(
        Point::from((panel.loc.x + PAD_X + NAME_PAD_X, panel.loc.y + NAME_PAD_TOP)),
        Size::from(((w - 2. * (PAD_X + NAME_PAD_X)).max(0.), name_h)),
    );

    // The name container has no bottom padding, so the view starts right under the label.
    let below = NAME_PAD_TOP + name_h;
    let grid_area = Rectangle::new(
        Point::from((panel.loc.x + PAD_X, panel.loc.y + below)),
        Size::from(((w - 2. * PAD_X).max(0.), (h - below).max(0.))),
    );

    DialogLayout {
        panel,
        name_band,
        grid_area,
    }
}

/// The folder currently on screen.
struct OpenFolder {
    /// The `folder-children` id — what the grid tile that opened it is keyed by.
    id: String,
    /// The already-translated display name (`_getFolderName`, `appDisplay.js:97-104`).
    name: String,
    /// The folder's own view: an [`AppGrid`] in its `FolderGrid` configuration.
    view: AppGrid,
}

#[derive(Default)]
struct DialogCache {
    /// The panel chrome — its rounded fill, inset border and name label. Keyed on the
    /// name and the panel size, so it survives paging and hovering inside the folder.
    panel: widget::BakeCache,
}

struct OutputData {
    shade: SolidColorBuffer,
}

/// The app-folder dialog. Owned on `Niri`; opened by a click on a folder tile.
pub struct FolderDialog {
    open: Option<OpenFolder>,
    cache: RefCell<DialogCache>,
    clock: Clock,
}

impl FolderDialog {
    pub fn new(clock: Clock) -> Self {
        Self {
            open: None,
            cache: RefCell::new(DialogCache::default()),
            clock,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The open folder's id, if any.
    pub fn folder_id(&self) -> Option<&str> {
        self.open.as_ref().map(|o| o.id.as_str())
    }

    /// Pop the dialog up on `id` with `members` (`FolderIcon.open`, `appDisplay.js:2334-2343`).
    /// Re-opening the folder that is already up is a no-op, as `popup` is when `_isOpen`.
    pub fn popup(&mut self, id: &str, name: &str, members: Vec<AppGridEntry>) {
        if self.folder_id() == Some(id) {
            return;
        }
        let mut view = AppGrid::folder_view(self.clock.clone());
        view.set_entries(members);
        self.open = Some(OpenFolder {
            id: id.to_owned(),
            name: name.to_owned(),
            view,
        });
    }

    /// Pop the dialog down. Returns whether it had been open (→ redraw, and the caller's
    /// Escape stops there rather than falling through to closing the grid).
    pub fn popdown(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// Re-seat the open folder against a fresh grid: `members` as they now resolve, or
    /// `None` if the folder is gone (emptied, deleted, or every member became a favorite),
    /// in which case the dialog goes with it — GNOME destroys the `FolderIcon`, and the
    /// dialog is destroyed with its source (`appDisplay.js:2320-2325`).
    pub fn resync(&mut self, members: Option<(String, Vec<AppGridEntry>)>) -> bool {
        let Some(open) = &mut self.open else {
            return false;
        };
        let Some((name, members)) = members else {
            self.open = None;
            return true;
        };
        let mut changed = open.view.set_entries(members);
        if open.name != name {
            open.name = name;
            changed = true;
        }
        changed
    }

    /// The id of the app at tile `i` inside the folder.
    pub fn entry_id(&self, i: usize) -> Option<&str> {
        self.open.as_ref()?.view.entry_id(i)
    }

    /// The member icons, for the startup decode prewarm.
    pub fn icon_refs(&self) -> impl Iterator<Item = &AppIconRef> {
        self.open.iter().flat_map(|o| o.view.icon_refs())
    }

    pub fn clear_icon_uploads(&self) {
        if let Some(open) = &self.open {
            open.view.clear_icon_uploads();
        }
    }

    pub fn drop_icon_upload(&self, icon: &AppIconRef, logical_px: u16) {
        if let Some(open) = &self.open {
            open.view.drop_icon_upload(icon, logical_px);
        }
    }

    /// What is at `pos` (logical, output coordinates). `None` when nothing is open — the
    /// dialog is modal, so once it *is* open every point on the output belongs to it.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        view: Rectangle<f64, Logical>,
    ) -> Option<DialogHit> {
        let open = self.open.as_ref()?;
        let l = layout(view);
        if !l.panel.contains(pos) {
            return Some(DialogHit::Outside);
        }
        if let Some(i) = open.view.hit_test(pos, l.grid_area) {
            return Some(DialogHit::App(i));
        }
        if let Some(page) = open.view.indicator_hit(pos, l.grid_area) {
            return Some(DialogHit::Page(page));
        }
        if let Some(arrow) = open.view.arrow_hit(pos, l.grid_area) {
            return Some(DialogHit::Arrow(arrow));
        }
        Some(DialogHit::Inside)
    }

    /// Track the pointer over the open folder. Returns whether anything changed (→ redraw).
    pub fn set_pointer(
        &mut self,
        pos: Option<Point<f64, Logical>>,
        view: Rectangle<f64, Logical>,
    ) -> bool {
        let l = layout(view);
        let Some(open) = &mut self.open else {
            return false;
        };
        let (tile, arrow) = match pos {
            Some(pos) => (
                open.view.hit_test(pos, l.grid_area),
                open.view.arrow_hit(pos, l.grid_area),
            ),
            None => (None, None),
        };
        let mut changed = open.view.set_hovered(tile);
        changed |= open.view.set_arrow_hovered(arrow);
        changed
    }

    /// Go to `page` of the open folder (a dot click); returns whether it moved.
    pub fn set_page(&mut self, page: usize, view: Rectangle<f64, Logical>) -> bool {
        let l = layout(view);
        let Some(open) = &mut self.open else {
            return false;
        };
        open.view.set_page(page, l.grid_area)
    }

    /// Step a page (a navigation-arrow click); returns whether it moved.
    pub fn step_page(&mut self, arrow: PageArrow, view: Rectangle<f64, Logical>) -> bool {
        let Some(open) = &self.open else {
            return false;
        };
        let cur = open.view.current_page();
        let target = match arrow {
            PageArrow::Prev => cur.saturating_sub(1),
            PageArrow::Next => cur + 1,
        };
        self.set_page(target, view)
    }

    pub fn current_page(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.view.current_page())
    }

    /// The logical center of the open folder's tile `k` on its current page — a geometry
    /// probe for the conformance corpus, which clicks real pixels.
    #[cfg(test)]
    pub fn tile_center(
        &self,
        k: usize,
        grid_area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.open.as_ref()?.view.tile_center(k, grid_area)
    }

    /// The same for tile `k`'s **icon** — the render test samples the drawn pixels there.
    #[cfg(test)]
    pub fn icon_center(
        &self,
        k: usize,
        grid_area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.open.as_ref()?.view.icon_center(k, grid_area)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        view: Rectangle<f64, Logical>,
        alpha: f32,
        push: &mut dyn FnMut(FolderDialogRenderElement),
    ) {
        let Some(open) = &self.open else { return };
        if alpha <= 0. {
            return;
        }
        let _span = tracy_client::span!("FolderDialog::render");

        let scale = output.current_scale().fractional_scale();
        let l = layout(view);

        // The folder's own grid, topmost (pushed first) — its tiles, captions, hover wash,
        // dots and arrows all come from the same widget the app grid uses.
        for element in open
            .view
            .render(renderer, app_icons, sym_icons, output, l.grid_area, alpha)
        {
            push(FolderDialogRenderElement::Texture(element));
        }

        // The panel itself: the rounded overlay surface, its inset hairline border, and the
        // folder name centered across the top.
        let panel_size = l.panel.size;
        let name = open.name.clone();
        let name_center = Point::from((
            l.name_band.loc.x - l.panel.loc.x + l.name_band.size.w / 2.,
            l.name_band.loc.y - l.panel.loc.y + l.name_band.size.h / 2.,
        ));
        let revision = widget::Revision::new()
            .of(&name)
            .px(panel_size.w)
            .px(panel_size.h)
            .done();
        match widget::bake(
            renderer,
            &mut self.cache.borrow_mut().panel,
            scale,
            panel_size,
            revision,
            move |r| {
                let mut shaper = widget::TextShaper::new(r, scale);
                shaper.shape(&name, widget::TextStyle::new(NAME_PT).bold())
            },
            move |frame, phys, label| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(RADIUS, style::OVERLAY_BG)?;
                p.stroke_rounded_full(RADIUS, BORDER_W, style::BORDERS)?;
                p.text(label, name_center, Align::CENTER, style::TEXT)?;
                Ok(())
            },
        ) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
                push(FolderDialogRenderElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        l.panel.loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
            Err(err) => tracing::error!("error baking the folder dialog: {err:#}"),
        }

        // The shade over everything behind, bottom-most.
        let size = view.size;
        let data = output.user_data().get_or_insert(|| {
            std::sync::Mutex::new(OutputData {
                shade: SolidColorBuffer::new(size, SHADE),
            })
        });
        let mut data = data.lock().unwrap();
        data.shade.resize(size);
        push(FolderDialogRenderElement::SolidColor(
            SolidColorRenderElement::from_buffer(&data.shade, view.loc, alpha, Kind::Unspecified),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)))
    }

    fn entry(id: &str) -> AppGridEntry {
        AppGridEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            icon: AppIconRef::Fallback,
            folder: None,
        }
    }

    fn open_dialog(n: usize) -> FolderDialog {
        let mut dialog = FolderDialog::new(Clock::with_time(std::time::Duration::ZERO));
        let members: Vec<_> = (0..n).map(|i| entry(&format!("app{i}.desktop"))).collect();
        dialog.popup("Utilities", "Utilities", members);
        dialog
    }

    /// The panel is `$app_folder_size` square, centered in the work area — the screen with
    /// the top panel's height taken off the top, which is what shifts it below center.
    #[test]
    fn the_panel_is_seven_hundred_and_twenty_square_centered_under_the_panel() {
        let l = layout(view());

        assert_eq!(l.panel.size, Size::from((720., 720.)));
        assert_eq!(l.panel.loc.x, (1920. - 720.) / 2.);
        // 35 + (1045 − 720)/2 = 35 + 162.5, rounded.
        assert_eq!(l.panel.loc.y, f64::round(35. + (1045. - 720.) / 2.));
    }

    /// The name band sits inside the horizontal padding, at the container's top padding, and
    /// the grid starts immediately under it — `.folder-name-container` has no bottom padding.
    #[test]
    fn the_name_band_pads_the_top_and_the_grid_starts_under_it() {
        let l = layout(view());
        let name_h = crate::ui::line_height_px(NAME_PT);

        assert_eq!(l.name_band.loc.x, l.panel.loc.x + 1. + 36.);
        assert_eq!(l.name_band.loc.y, l.panel.loc.y + 24.);
        assert_eq!(l.name_band.size.w, 720. - 2. * (1. + 36.));

        assert_eq!(l.grid_area.loc.x, l.panel.loc.x + 1.);
        assert_eq!(l.grid_area.loc.y, l.panel.loc.y + 24. + name_h);
        assert_eq!(l.grid_area.size.h, 720. - 24. - name_h);
    }

    /// A screen too short for 720 + the panel strut clamps the dialog rather than letting it
    /// run off the bottom, and it still starts below the panel.
    #[test]
    fn a_short_screen_clamps_the_panel_into_the_work_area() {
        let l = layout(Rectangle::new(
            Point::from((0., 0.)),
            Size::from((1024., 600.)),
        ));

        // Each axis clamps on its own — `width`/`height` are separate declarations, so a
        // screen short in one direction only does not shrink the other.
        assert_eq!(l.panel.size, Size::from((720., 600. - 35.)));
        assert_eq!(l.panel.loc.y, 35.);
        assert!(l.panel.loc.y + l.panel.size.h <= 600.);
    }

    /// A click inside the panel is consumed; a click anywhere else pops the dialog down —
    /// the whole of GNOME's click gesture. And with nothing open the dialog is transparent
    /// to hit-testing, so the grid behind it keeps its clicks.
    #[test]
    fn outside_the_panel_pops_down_and_a_closed_dialog_hits_nothing() {
        let dialog = open_dialog(4);
        let l = layout(view());

        assert_eq!(
            dialog.hit_test(Point::from((5., 5.)), view()),
            Some(DialogHit::Outside)
        );
        let corner = Point::from((l.panel.loc.x + 4., l.panel.loc.y + 4.));
        assert_eq!(dialog.hit_test(corner, view()), Some(DialogHit::Inside));

        let closed = FolderDialog::new(Clock::with_time(std::time::Duration::ZERO));
        assert_eq!(closed.hit_test(corner, view()), None);
    }

    /// The inner view is a 3×3 `FolderGrid`: nine apps fit one page, the tenth makes a second.
    #[test]
    fn the_folder_view_paginates_three_by_three() {
        let l = layout(view());

        let dialog = open_dialog(9);
        let open = dialog.open.as_ref().unwrap();
        assert_eq!(open.view.page_count(l.grid_area), 1);
        assert_eq!(open.view.visible_len(l.grid_area), 9);

        let dialog = open_dialog(10);
        let open = dialog.open.as_ref().unwrap();
        assert_eq!(open.view.page_count(l.grid_area), 2);
        assert_eq!(open.view.visible_len(l.grid_area), 9);
    }

    /// Every tile is hittable and resolves to its own app, and a hit maps back to the id the
    /// caller launches.
    #[test]
    fn each_tile_hits_its_own_app() {
        let dialog = open_dialog(9);
        let l = layout(view());
        let open = dialog.open.as_ref().unwrap();

        for i in 0..9 {
            let center = open.view.entry_center(i, l.grid_area).unwrap();
            assert_eq!(
                dialog.hit_test(center, view()),
                Some(DialogHit::App(i)),
                "tile {i}"
            );
            assert_eq!(dialog.entry_id(i), Some(format!("app{i}.desktop").as_str()));
        }
    }

    /// Re-opening the folder that is already up keeps its page — `popup` is a no-op while
    /// `_isOpen`, so a second click on the same tile must not reset the view.
    #[test]
    fn reopening_the_same_folder_keeps_its_page() {
        let mut dialog = open_dialog(10);
        assert!(dialog.set_page(1, view()));
        assert_eq!(dialog.current_page(), 1);

        dialog.popup("Utilities", "Utilities", vec![entry("app0.desktop")]);
        assert_eq!(dialog.current_page(), 1);

        // A *different* folder replaces the view outright.
        dialog.popup("System", "System", vec![entry("app0.desktop")]);
        assert_eq!(dialog.current_page(), 0);
        assert_eq!(dialog.folder_id(), Some("System"));
    }

    /// A resync that no longer finds the folder closes the dialog; one that does keeps it
    /// open with the new members.
    #[test]
    fn a_vanished_folder_takes_the_dialog_with_it() {
        let mut dialog = open_dialog(4);
        assert!(dialog.resync(Some(("Utilities".to_owned(), vec![entry("only.desktop")]))));
        assert!(dialog.is_open());
        assert_eq!(dialog.entry_id(0), Some("only.desktop"));
        assert_eq!(dialog.entry_id(1), None);

        assert!(dialog.resync(None));
        assert!(!dialog.is_open());
    }
}
