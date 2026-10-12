// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The login dialog's administrator-set chrome (`js/gdm/loginDialog.js`, `docs/fork/greeter.md`
//! §4.2): the banner over the prompt (`banner-message-*`) and the logo at the bottom (`logo`).

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::image_source::ImageSource;
use crate::render_helpers::icon::ImageCache;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::ui::lock_screen::{PageCtx, PageTransform, PromptElement};
use crate::ui::widget::{self, style, Align, Painter, Rgba, TextShaper, TextStyle};

/// `$_gdm_dialog_width` (`_login-lock.scss:3`), the banner view's `max-width`, in em.
const BANNER_MAX_W_EM: f64 = 25.;
/// `.login-dialog-banner`'s `darken($_gdm_fg, 10%)` (`_login-lock.scss:86-88`): `#fafafb` at 10%
/// less lightness.
const BANNER_FG: Rgba = [
    0xde as f32 / 255.,
    0xde as f32 / 255.,
    0xe4 as f32 / 255.,
    1.,
];
/// `.login-dialog-logo-bin`'s `margin: 3em 0` (`_login-lock.scss:79-81`).
const LOGO_MARGIN_EM: f64 = 3.;

/// The logo file and its natural size, read when the setting is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logo {
    pub path: std::path::PathBuf,
    pub size: (u32, u32),
}

impl Logo {
    /// `None` when the file cannot be read as an image: gnome-shell shows nothing then either.
    pub fn load(path: &str) -> Option<Self> {
        if path.is_empty() {
            return None;
        }
        match image::image_dimensions(path) {
            Ok(size) => Some(Self {
                path: path.into(),
                size,
            }),
            Err(err) => {
                tracing::warn!("login screen logo {path}: {err}");
                None
            }
        }
    }
}

fn base_px() -> f64 {
    crate::ui::pt_to_px(crate::ui::base_font_pt())
}

/// The banner's lines: each paragraph wrapped at the view's `max-width`
/// (`line_wrap`, `ellipsize = NONE`, `loginDialog.js:629-636`).
pub fn banner_lines(text: &str) -> Vec<String> {
    let px = base_px();
    let max_w = BANNER_MAX_W_EM * px;
    text.trim_end()
        .split('\n')
        .flat_map(|paragraph| {
            if paragraph.trim().is_empty() {
                vec![String::new()]
            } else {
                synoik_vk::text::wrap_lines_weighted(
                    paragraph,
                    px as f32,
                    false,
                    max_w,
                    usize::MAX,
                    true,
                )
            }
        })
        .collect()
}

/// The banner's box for `lines`: the view's whole `max-width` (or the monitor's, if narrower),
/// each line centred in it. gnome-shell sizes the view to its text and centres the view; centring
/// the lines in a fixed box draws the same, and cannot cut a line whose shaped width differs from
/// a measured one.
fn banner_natural(lines: &[String], monitor_w: f64) -> Size<f64, Logical> {
    let w = (BANNER_MAX_W_EM * base_px()).min(monitor_w);
    let h = lines.len() as f64 * crate::ui::line_height_px(crate::ui::base_font_pt());
    Size::from((w, h))
}

/// Where the banner goes (`vfunc_allocate`, `loginDialog.js:754-858`): centred, below the panel,
/// then moved down into the middle of the room left above the prompt. When it does not fit there it
/// gets the top half of that room and is cut at its bottom. Not built: gnome-shell's wide layout,
/// banner and prompt side by side, nor the banner's scroll bar.
pub fn banner_rect(
    monitor: Rectangle<f64, Logical>,
    panel_h: f64,
    prompt_top: f64,
    natural: Size<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let x = (monitor.loc.x + monitor.size.w / 2. - natural.w / 2.).floor();
    let mut y = monitor.loc.y + panel_h;
    let mut h = natural.h;
    let left_over = (prompt_top - y) - h;
    if left_over > 0. {
        y += (left_over / 2. / 2.).floor();
    } else {
        h = ((left_over + h) / 2.).floor().max(0.);
    }
    Rectangle::new(Point::from((x, y)), Size::from((natural.w, h)))
}

/// Where the logo goes (`_getLogoBinAllocation`, `loginDialog.js:705-717`): centred at the bottom,
/// above its margin.
pub fn logo_rect(monitor: Rectangle<f64, Logical>, logo: &Logo) -> Rectangle<f64, Logical> {
    let (w, h) = (f64::from(logo.size.0), f64::from(logo.size.1));
    let margin = LOGO_MARGIN_EM * base_px();
    Rectangle::new(
        Point::from((
            (monitor.loc.x + monitor.size.w / 2. - w / 2.).floor(),
            monitor.loc.y + monitor.size.h - margin - h,
        )),
        Size::from((w, h)),
    )
}

/// The two pieces' caches.
#[derive(Default)]
pub struct LoginDialogChrome {
    banner: RefCell<widget::BakeCache>,
    logo: RefCell<widget::ImageUploads>,
}

impl LoginDialogChrome {
    /// The banner, shown with the prompt and not with the user list (`_showPrompt` /
    /// `_showUserList`, `loginDialog.js:1115-1126`, `1470-1478`). `prompt_top` is where the prompt
    /// block starts.
    pub fn render_banner(
        &self,
        renderer: &mut VulkanRenderer,
        ctx: PageCtx,
        text: &str,
        prompt_top: f64,
        t: PageTransform,
    ) -> Option<PromptElement> {
        let PageCtx { scale, monitor, .. } = ctx;
        let lines = banner_lines(text);
        let natural = banner_natural(&lines, monitor.size.w);
        let rect = banner_rect(
            monitor,
            crate::ui::panel::panel_height(),
            prompt_top,
            natural,
        );
        if rect.size.w <= 0. || rect.size.h <= 0. {
            return None;
        }
        let line_h = crate::ui::line_height_px(crate::ui::base_font_pt());
        let size = rect.size;
        let baked = widget::bake(
            renderer,
            &mut self.banner.borrow_mut(),
            scale,
            size,
            widget::Revision::new()
                .of(text)
                .px(size.w)
                .px(size.h)
                .done(),
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                lines
                    .iter()
                    .map(|l| shaper.shape(l, TextStyle::new(crate::ui::base_font_pt())))
                    .collect::<anyhow::Result<Vec<_>>>()
            },
            |frame, phys, runs| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                // Lines past the allocation are cut, as a `ScrollView` without its bar would show.
                for (i, run) in runs.iter().enumerate() {
                    let top = i as f64 * line_h;
                    if top >= size.h {
                        break;
                    }
                    p.text(
                        run,
                        Point::from((size.w / 2., top + line_h / 2.)),
                        Align::CENTER,
                        BANNER_FG,
                    )?;
                }
                Ok(())
            },
        );
        let centre = Point::from((
            monitor.loc.x + monitor.size.w / 2.,
            monitor.loc.y + monitor.size.h / 2.,
        ));
        match baked {
            Ok(texture) => {
                let mut buffer = texture;
                buffer.set_texture_scale(t.buffer_scale(scale));
                Some(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        t.place(rect.loc, centre),
                        t.alpha as f32,
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .into(),
                )
            }
            Err(err) => {
                tracing::error!("error drawing the login banner: {err:#}");
                None
            }
        }
    }

    /// The logo, on every page of the dialog.
    pub fn render_logo(
        &self,
        renderer: &mut VulkanRenderer,
        images: &ImageCache,
        ctx: PageCtx,
        logo: &Logo,
        t: PageTransform,
    ) -> Option<PromptElement> {
        let PageCtx { scale, monitor, .. } = ctx;
        let rect = logo_rect(monitor, logo);
        let centre = Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.));
        // Fitted into a square as wide as its longer side, transparent around it: centred, that
        // is the picture at its natural size.
        let side = rect.size.w.max(rect.size.h);
        widget::image_element(
            renderer,
            &mut self.logo.borrow_mut(),
            images,
            &ImageSource::File(logo.path.clone()),
            0,
            side,
            scale,
            Point::from((0., 0.)),
            centre,
            t.alpha as f32,
        )
        .map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon() -> Rectangle<f64, Logical> {
        Rectangle::from_size(Size::from((1280., 800.)))
    }

    /// With room above the prompt, the banner moves a quarter of the spare room down
    /// (`loginDialog.js:801-812`); without, it keeps the top half and is cut.
    #[test]
    fn the_banner_takes_the_room_above_the_prompt() {
        let natural = Size::from((200., 40.));
        let r = banner_rect(mon(), 32., 266., natural);
        assert_eq!(r.loc.x, 540.);
        // 266 - 32 - 40 = 194 spare; a quarter of it is 48.
        assert_eq!(r.loc.y, 32. + 48.);
        assert_eq!(r.size.h, 40.);

        let tall = Size::from((200., 400.));
        let r = banner_rect(mon(), 32., 266., tall);
        assert_eq!(r.loc.y, 32.);
        assert_eq!(
            r.size.h, 117.,
            "half of the 234 between the panel and the prompt"
        );
    }

    #[test]
    fn the_logo_sits_centred_above_its_bottom_margin() {
        let logo = Logo {
            path: "/x.png".into(),
            size: (149, 43),
        };
        let r = logo_rect(mon(), &logo);
        assert_eq!(r.loc.x, (640. - 74.5_f64).floor());
        assert_eq!(r.loc.y + r.size.h, 800. - 3. * base_px());
    }

    #[test]
    fn paragraphs_keep_their_breaks_and_long_ones_wrap() {
        let lines = banner_lines("Authorised use only.\n\nThis system is monitored.\n");
        assert_eq!(
            lines,
            ["Authorised use only.", "", "This system is monitored."]
        );
        let long = "word ".repeat(60);
        assert!(banner_lines(&long).len() > 1);
    }
}
