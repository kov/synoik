// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The overview thumbnail strip's per-thumbnail chrome: the close button an empty
//! workspace grows while the pointer is on it.
//!
//! **Divergence** (`docs/fork/dynamic-workspaces-divergence.md`). gnome-shell has no such
//! button: `WorkspaceTracker._checkWorkspaces` reaps empty workspaces
//! (`js/ui/windowManager.js:278-291`), so there is never one sitting around to dismiss.
//! We keep them and let you close them by hand, Mission Control's model.
//!
//! Geometry lives in [`crate::layout::thumbnails`] with the rest of the strip's; this
//! module is the paint side. The button is [`widget::IconButton`], the toolkit's circular
//! glyph button, so its hover wash and its round hit test are the ones every other icon
//! button in the shell uses.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Rectangle, Size};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::widget::{self, style, BakeCache, Painter, Rgba, TextShaper, TextStyle};

/// The glyph inside the button. The same one the window picker's close button uses, so the
/// two "dismiss this" affordances in the overview read as one control at two sizes.
const CLOSE_ICON: &str = "preview-close-symbolic";

/// The glyph's share of the button's diameter — [`widget::IconButton::diameter`] read
/// backwards, since here the diameter comes from the strip's geometry (it is ramped with
/// the rest of the overview chrome) and the glyph has to follow it.
const ICON_RATIO: f64 = 2. / 3.;

/// `$window_close_button_color` (`_window-picker.scss:2`) — see
/// [`crate::ui::window_preview`], which resolves the same SCSS for the preview's button.
const CLOSE_BG: Rgba = [0.2471, 0.2471, 0.2745, 0.98];

/// One thumbnail's name label, as the renderer needs it.
#[derive(Debug, Clone)]
pub struct ThumbnailName {
    /// The thumbnail's box, in view coordinates — the pill is placed inside it.
    pub thumb: Rectangle<f64, Logical>,
    /// The name, already ellipsized to what the thumbnail can hold.
    pub name: String,
}

/// Everything the strip draws over its thumbnails in one frame.
pub struct StripChrome<'a> {
    /// The close button an empty workspace grows while hovered.
    pub buttons: &'a [ThumbnailClose],
    /// Every named workspace's label — minus the one being renamed, which wears the entry.
    pub names: &'a [ThumbnailName],
    /// The name entry, when a rename is in progress.
    pub entry: Option<ThumbnailEntry<'a>>,
}

/// The name entry, while a workspace is being renamed.
pub struct ThumbnailEntry<'a> {
    /// The thumbnail's box, in view coordinates.
    pub thumb: Rectangle<f64, Logical>,
    /// The editing model — text, caret and selection all come from it.
    pub edit: &'a crate::ui::text_edit::TextEdit,
}

/// One thumbnail's close button, as the renderer needs it.
#[derive(Debug, Clone, Copy)]
pub struct ThumbnailClose {
    /// The button's box, in view coordinates.
    pub rect: Rectangle<f64, Logical>,
    /// How far the hover has faded it in. 0 draws nothing.
    pub alpha: f32,
    /// Whether the pointer is on the button itself, for its hover wash.
    pub hovered: bool,
}

/// The strip chrome's GPU caches: one disc bake per (size, hover) key, and one pill bake per
/// distinct label.
#[derive(Default)]
pub struct ThumbnailChrome {
    disc: RefCell<BakeCache>,
    names: RefCell<std::collections::HashMap<String, BakeCache>>,
    entry: RefCell<BakeCache>,
}

impl ThumbnailChrome {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop the bakes for labels no thumbnail is showing any more, so a renamed or closed
    /// workspace does not leave its pill in the cache for the session.
    pub fn retain_names(&self, names: &[ThumbnailName]) {
        self.names
            .borrow_mut()
            .retain(|key, _| names.iter().any(|n| n.name == *key));
    }

    /// Render the close buttons and the name labels, topmost first (the caller pushes these
    /// over the strip).
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        accent: Rgba,
        chrome: StripChrome<'_>,
    ) -> Vec<ThumbnailChromeRenderElement> {
        let StripChrome {
            buttons,
            names,
            entry,
        } = chrome;
        let mut elements: Vec<ThumbnailChromeRenderElement> = Vec::new();

        // The name entry, where that workspace's label would be: what is being edited is where
        // the result will show.
        if let Some(entry) = entry {
            let rect = crate::ui::workspace_rename::entry_rect(entry.thumb);
            let content = widget::EntryContent::of(entry.edit, "Name this workspace", true);
            let rev = widget::Revision::new()
                .of(content.text)
                .of(content.cursor)
                .of(content.selection.clone())
                .of(content.preedit)
                .px(rect.size.w)
                .done();
            let baked = widget::Entry::bake(
                renderer,
                &mut self.entry.borrow_mut(),
                scale,
                rect.size.w,
                rect.size.h,
                content,
                crate::ui::workspace_rename::STYLE,
                true,
                false,
                // Unused by this family: over a wallpaper the focus ring is white, not the
                // accent, for the reason the lock screen's is.
                style::TEXT,
                rev,
            );
            match baked {
                Ok(buffer) => elements.push(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        rect.loc,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .into(),
                ),
                Err(err) => tracing::error!("error baking the workspace name entry: {err:#}"),
            }
        }

        // The same caption pill the window picker puts under a preview, so the two labels in
        // the overview read as one kind of overlay.
        for label in names {
            if label.name.is_empty() {
                continue;
            }
            let size = widget::Tooltip::size(&label.name);
            let mut caches = self.names.borrow_mut();
            let cache = caches.entry(label.name.clone()).or_default();
            let pill = widget::bake(
                renderer,
                cache,
                scale,
                size,
                0,
                |r| {
                    let mut shaper = TextShaper::new(r, scale);
                    shaper.shape(&label.name, TextStyle::new(widget::Tooltip::TEXT_PT))
                },
                |frame, phys, text| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.tooltip(size, text)
                },
            );
            match pill {
                Ok(buffer) => elements.push(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        crate::layout::thumbnails::name_rect(label.thumb, size).loc,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .into(),
                ),
                Err(err) => tracing::error!("error baking a workspace name label: {err:#}"),
            }
        }

        for button in buttons {
            if button.alpha <= 0. {
                continue;
            }
            let size = button.rect.size.w.min(button.rect.size.h);
            let icon_px = (size * ICON_RATIO).round();

            // The glyph goes on top of the disc, so it is pushed first.
            if let Some(el) = widget::icon_element_alpha(
                renderer,
                icons,
                &[CLOSE_ICON],
                icon_px,
                scale,
                style::TEXT,
                button.rect.loc,
                button.rect.size.downscale(2.).to_point(),
                button.alpha,
            ) {
                elements.push(el.into());
            }

            // The circle, baked in its own local space so the texture is
            // position-independent and the cache key is just its size and hover state.
            let local = Rectangle::from_size(Size::from((size, size)));
            let widget_button =
                widget::IconButton::new(local, icon_px, CLOSE_BG).hovered(button.hovered);
            let disc = widget::bake(
                renderer,
                &mut self.disc.borrow_mut(),
                scale,
                local.size,
                widget::Revision::new().of(button.hovered).px(size).done(),
                |_| Ok(()),
                |frame, phys, ()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    p.icon_button(&widget_button, accent)
                },
            );
            if let Ok(buffer) = disc {
                elements.push(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        button.rect.loc,
                        button.alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .into(),
                );
            }
        }

        elements
    }
}

synoik_render_elements! {
    ThumbnailChromeRenderElement => {
        Texture = TextureRenderElement<VkTexture>,
    }
}
