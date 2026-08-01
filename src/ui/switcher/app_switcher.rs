//! `AppSwitcherPopup` / `AppSwitcher` (`js/ui/altTab.js:62-140, 696-908`) — Super-Tab.
//!
//! One item per **application**, not per window: a 96px app icon over the app's name, with a
//! small chevron under any app that has more than one window open.
//!
//! The two orderings here come from different places, which is easy to miss and changes what the
//! second item is:
//! - **items** are `Shell.AppSystem.get_default().get_running()` (`:73`), sorted by
//!   `shell_app_compare` — our [`AppSystem::running`](crate::app_system::AppSystem::running)
//!   already reproduces that;
//! - **each app's windows** come from the tab list, filtered to that app (`:717-724`), so within an
//!   app they are in focus-recency order rather than the app's own order.
//!
//! An app's window list is also *cached at open* — the comment at `:719-720` says so explicitly
//! ("we don't handle dynamic changes here"). Windows opening while the popup is up do not appear.

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::app_system::RunningApp;
use crate::ui::switcher::{ITEM_PADDING, ITEM_SPACING, LIST_PADDING};
use crate::window::mapped::MappedId;

/// `APP_ICON_SIZE` (`altTab.js:20`) — the size an app icon is drawn at when everything fits.
pub const APP_ICON_SIZE: f64 = 96.;

/// `baseIconSizes` (`altTab.js:23`) — the ladder `_setIconSize` walks down until the row fits.
///
/// The same shape as the app grid's `_findBestIconSize`, which we already port, but a fixed
/// ladder rather than a search: an app switcher with many apps ends up at 22px icons rather than
/// scrolling.
pub const ICON_SIZES: [f64; 5] = [96., 64., 48., 32., 22.];

/// `APP_ICON_HOVER_TIMEOUT` (`altTab.js:13`) — while the thumbnail sub-list is open, hovering a
/// different app waits this long before switching to it (`:812-841`), so dragging the pointer
/// across the row does not flash every app's thumbnails on the way. Slice 5 needs it; recorded
/// here with the rest of the app switcher's timing.
pub const ICON_HOVER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// `org.gnome.shell.app-switcher current-workspace-only`
/// (`data/org.gnome.shell.gschema.xml.in:307-318`), whose default is **false** — the app switcher
/// spans workspaces where the window switcher does not.
///
/// NOT YET READ FROM GSETTINGS: we carry the default. Wiring it means adding the schema to the
/// `GnomeSettings` model, which is a self-contained follow-up rather than part of this port —
/// tracked in `docs/fork/alt-tab-port.md`. Until then a user who flips the key sees no change.
pub const CURRENT_WORKSPACE_ONLY: bool = false;

/// One row item: an app, and the windows of it that the switcher will cycle through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppItem {
    pub app_id: String,
    /// This app's windows **in tab-list order**, not the app's own order.
    pub windows: Vec<MappedId>,
}

impl AppItem {
    /// Whether this app gets a chevron — `_addIcon` hides the arrow for a single window
    /// (`altTab.js:889-892`).
    pub fn has_arrow(&self) -> bool {
        self.windows.len() > 1
    }
}

/// Group `tab_list` under `running`, dropping apps with nothing on screen.
///
/// Item order is `running`'s (i.e. `shell_app_compare`); window order within an item is
/// `tab_list`'s. Apps whose windows are all filtered out — by `current-workspace-only`, or
/// because they are starting and have no window yet — are skipped entirely (`:723-724`), so the
/// switcher never shows an app you cannot switch to.
pub fn app_items(running: &[RunningApp], tab_list: &[MappedId]) -> Vec<AppItem> {
    running
        .iter()
        .filter_map(|app| {
            let owned: Vec<MappedId> = app.windows.iter().map(|w| w.id).collect();
            let windows: Vec<MappedId> = tab_list
                .iter()
                .copied()
                .filter(|id| owned.contains(id))
                .collect();

            (!windows.is_empty()).then(|| AppItem {
                app_id: app.id.clone(),
                windows,
            })
        })
        .collect()
}

/// Pick the icon size — `AppSwitcher._setIconSize` (`altTab.js:742-782`).
///
/// The item is square (see [`PanelLayout`](super::PanelLayout)), and its side is the icon plus
/// `iconSpacing` = the label's height plus the item box's horizontal padding and border
/// (`:750-753`). So the fit test is `side * n + spacing * (n-1) <= available`, walking the ladder
/// until it passes.
///
/// Note the ladder is walked with `break`, not `continue`: if even the smallest rung overflows,
/// the loop still ends on 22 and the row overflows rather than scrolling. GNOME accepts that.
///
/// **A single item never shrinks** (`:762`) — one app is always drawn at 96px however narrow the
/// monitor is.
///
/// DIVERGENCE: GNOME compares `baseIconSizes[i] * scale_factor` against a *logical* available
/// width (`:759-760, 765-770`), so on an integer-scaled HiDPI monitor it drops a rung earlier
/// than the arithmetic suggests. We have no integer `scale_factor` — output scale is applied by
/// the renderer and can be fractional — so the comparison here is wholly logical. Reproducing
/// GNOME's mixed-unit comparison would mean inventing a scale factor we do not otherwise have.
pub fn icon_size(item_count: usize, label_height: f64, available_width: f64) -> f64 {
    if item_count <= 1 {
        return ICON_SIZES[0];
    }

    // `iconSpacing`: the label under the icon, plus the item box's own padding.
    let icon_spacing = label_height + ITEM_PADDING * 2.;
    let n = item_count as f64;
    let total_spacing = ITEM_SPACING * (n - 1.);

    for size in ICON_SIZES {
        let side = size + icon_spacing;
        if side * n + total_spacing <= available_width {
            return size;
        }
    }

    ICON_SIZES[ICON_SIZES.len() - 1]
}

/// The width a row of `item_count` items has to fit into — the monitor less the panel's own
/// padding (`altTab.js:757-760`).
///
/// GNOME also subtracts the parent `.switcher-popup`'s horizontal padding, which is `0`
/// (`_switcher-popup.scss:9`).
pub fn available_width(monitor_width: f64) -> f64 {
    monitor_width - LIST_PADDING * 2.
}

/// The chevron under a multi-window app — `AppSwitcher.vfunc_allocate` (`altTab.js:793-809`).
///
/// Its size is derived from the panel's own bottom padding rather than being its own constant:
/// `arrowHeight = floor(padding_bottom / 3)` and `arrowWidth = arrowHeight * 2` (`:796-797`).
/// With [`LIST_PADDING`] of 12 that is a 8x4 triangle.
///
/// It sits *below* the item box, inside the panel's bottom padding, centered on the item — which
/// is why the panel does not need to grow to make room for it.
pub fn arrow_rect(item: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let height = (LIST_PADDING / 3.).floor();
    let width = height * 2.;

    Rectangle::new(
        Point::from((
            item.loc.x + ((item.size.w - width) / 2.).floor(),
            item.loc.y + item.size.h + height,
        )),
        Size::from((width, height)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_system::RunningWindow;

    fn running(id: &str, windows: &[MappedId]) -> RunningApp {
        RunningApp {
            id: id.to_owned(),
            windows: windows
                .iter()
                .map(|&id| RunningWindow {
                    id,
                    app_id: None,
                    title: None,
                    last_focus: None,
                })
                .collect(),
            last_focus: None,
        }
    }

    /// Items follow the running-app order, but each app's windows follow the tab list.
    ///
    /// The two orders come from different places (`altTab.js:73` vs `:717-724`), and mixing them
    /// up is invisible until an app's *second* window is the one you wanted: the thumbnail
    /// sub-list and `_nextWindow` both index into this order.
    #[test]
    fn items_take_the_app_order_and_windows_take_the_tab_order() {
        let (a1, a2, b1) = (MappedId::next(), MappedId::next(), MappedId::next());

        // The app lists its windows one way...
        let apps = [running("a.desktop", &[a1, a2]), running("b.desktop", &[b1])];
        // ...while the tab list has a2 ahead of a1.
        let items = app_items(&apps, &[b1, a2, a1]);

        assert_eq!(
            items[0].app_id, "a.desktop",
            "app order is the running order"
        );
        assert_eq!(items[1].app_id, "b.desktop");
        assert_eq!(items[0].windows, [a2, a1], "window order is the tab order");
    }

    /// An app with no windows in the list is not shown at all.
    ///
    /// This is what `current-workspace-only` does to the app switcher, and it is also why a
    /// starting app with no window yet cannot appear: there would be nothing to switch to.
    #[test]
    fn apps_with_no_visible_windows_are_dropped() {
        let (a1, b1) = (MappedId::next(), MappedId::next());
        let apps = [running("a.desktop", &[a1]), running("b.desktop", &[b1])];

        // Only b's window is on the active workspace.
        let items = app_items(&apps, &[b1]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].app_id, "b.desktop");
    }

    /// The chevron marks an app you can descend into.
    #[test]
    fn only_multi_window_apps_get_an_arrow() {
        let (a1, a2, b1) = (MappedId::next(), MappedId::next(), MappedId::next());
        let apps = [running("a.desktop", &[a1, a2]), running("b.desktop", &[b1])];
        let items = app_items(&apps, &[a1, a2, b1]);

        assert!(items[0].has_arrow());
        assert!(!items[1].has_arrow());
    }

    /// The ladder drops a rung only when the row would not fit.
    #[test]
    fn the_icon_ladder_shrinks_until_the_row_fits() {
        let label = 20.;
        let spacing = label + ITEM_PADDING * 2.;

        // Room for four 96px items: side = 96 + spacing, plus three gaps.
        let side = 96. + spacing;
        let roomy = side * 4. + ITEM_SPACING * 3.;
        assert_eq!(icon_size(4, label, roomy), 96.);

        // One logical pixel short, and it takes the next rung down.
        assert_eq!(icon_size(4, label, roomy - 1.), 64.);

        // A very narrow monitor bottoms out rather than going below the ladder.
        assert_eq!(icon_size(20, label, 200.), 22.);
    }

    /// One app is drawn at full size however narrow the screen is (`altTab.js:762`).
    ///
    /// The loop is guarded by `_items.length > 1`, so the single-item case keeps the initial 96
    /// rather than measuring at all.
    #[test]
    fn a_lone_app_never_shrinks() {
        assert_eq!(icon_size(1, 20., 10.), 96.);
        assert_eq!(icon_size(0, 20., 10.), 96.);
    }

    /// The arrow is derived from the panel's bottom padding, and lives inside it.
    ///
    /// Worth pinning as a *relation* rather than as 8x4: it is `floor(padding/3)` by 2, so it
    /// tracks the panel padding instead of drifting from it (`altTab.js:796-797`).
    #[test]
    fn the_arrow_sits_in_the_panels_bottom_padding() {
        let item = Rectangle::new(Point::from((100., 50.)), Size::from((120., 120.)));
        let arrow = arrow_rect(item);

        let height = (LIST_PADDING / 3.).floor();
        assert_eq!(arrow.size, Size::from((height * 2., height)));

        // Centered under the item...
        assert_eq!(
            arrow.loc.x + arrow.size.w / 2.,
            item.loc.x + item.size.w / 2.
        );
        // ...and below it, still within the padding the panel already reserves.
        assert!(arrow.loc.y >= item.loc.y + item.size.h);
        assert!(arrow.loc.y + arrow.size.h <= item.loc.y + item.size.h + LIST_PADDING);
    }
}
