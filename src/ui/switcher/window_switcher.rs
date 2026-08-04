// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `WindowSwitcherPopup` / `WindowSwitcher` (`js/ui/altTab.js:580-640, 1002-1060`) — Alt-Tab.
//!
//! One item per **window**: a live 128px preview with the app's icon tucked into its
//! bottom-right corner, and the window title underneath.
//!
//! The default that catches people out is the workspace filter. `org.gnome.shell.window-switcher
//! current-workspace-only` defaults to **true** where the app switcher's defaults to false, so
//! stock Alt-Tab shows only this workspace while stock Super-Tab spans all of them. On a
//! one-workspace machine the difference is invisible, which is exactly why it is worth stating.

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::ui::switcher::ITEM_PADDING;

/// `WINDOW_PREVIEW_SIZE` (`altTab.js:19`) — the square each preview is centered in.
///
/// A *box* size, not the preview's own size: the clone is fitted inside it preserving aspect and
/// never upscaled (see [`window_thumbnail`](crate::render_helpers::window_thumbnail)), so most
/// previews are letterboxed and a small window sits tiny in the middle of its square.
pub const WINDOW_PREVIEW_SIZE: f64 = 128.;

/// `APP_ICON_SIZE_SMALL` (`altTab.js:21`) — the app badge over the preview.
pub const APP_ICON_SIZE_SMALL: f64 = 48.;

/// The preview's box inside an item — the whole content area, since the icon overlays it rather
/// than sitting beside it.
///
/// `WindowIcon` puts the clone and the app icon in the same `St.Bin` under a `Clutter.BinLayout`
/// (`altTab.js:1013, 1029-1039`), and sizes that bin to `WINDOW_PREVIEW_SIZE` square (`:1047`).
pub fn preview_box(item: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((item.loc.x + ITEM_PADDING, item.loc.y + ITEM_PADDING)),
        Size::from((WINDOW_PREVIEW_SIZE, WINDOW_PREVIEW_SIZE)),
    )
}

/// Where the app icon's centre goes: the **bottom-right** of the preview box.
///
/// `_createAppIcon` sets `x_align = y_align = Clutter.ActorAlign.END` (`altTab.js:1055-1056`), so
/// the icon is flush with the box's far corner rather than overhanging it — unlike the window
/// picker's close button, which is deliberately centred *on* the corner. Getting this wrong is
/// invisible on a preview that fills its box and obvious on one that does not, because the icon
/// tracks the box, not the clone.
pub fn app_icon_center(preview: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    Point::from((
        preview.loc.x + preview.size.w - APP_ICON_SIZE_SMALL / 2.,
        preview.loc.y + preview.size.h - APP_ICON_SIZE_SMALL / 2.,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The badge sits in the corner of the *box*, not of the clone.
    #[test]
    fn the_app_icon_is_flush_with_the_previews_bottom_right() {
        let item = Rectangle::new(
            Point::from((100., 200.)),
            Size::from((
                WINDOW_PREVIEW_SIZE + ITEM_PADDING * 2.,
                WINDOW_PREVIEW_SIZE + ITEM_PADDING * 2.,
            )),
        );
        let preview = preview_box(item);
        let center = app_icon_center(preview);

        // Flush inside the corner: the icon's far edges land exactly on the box's.
        assert_eq!(
            center.x + APP_ICON_SIZE_SMALL / 2.,
            preview.loc.x + preview.size.w
        );
        assert_eq!(
            center.y + APP_ICON_SIZE_SMALL / 2.,
            preview.loc.y + preview.size.h
        );

        // And fully inside it — an END-aligned child does not overhang, which is what separates
        // this from the window picker's close button.
        assert!(center.x - APP_ICON_SIZE_SMALL / 2. >= preview.loc.x);
        assert!(center.y - APP_ICON_SIZE_SMALL / 2. >= preview.loc.y);
    }
}
