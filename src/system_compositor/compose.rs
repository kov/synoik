// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The outer's own drawing: what it composites during a session switch (§5), and the boot splash
//! (`docs/fork/boot-splash.md` §4).
//!
//! A switch is two pictures, crossfaded into one of the outer's own scanout buffers. A picture is
//! either a flat colour (the plate) or a texture: a snapshot the outer took of what was on screen,
//! or a session frame imported from its dmabuf. Steady state never comes here — a fronted
//! session's buffers are flipped as they are.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;

use anyhow::{anyhow, Context as _};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::{Allocator as _, Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::{DrmDeviceFd, DrmNode};
use smithay::backend::renderer::{
    Bind as _, Color32F, Frame as _, ImportDma as _, ImportMem as _, Offscreen as _, Renderer as _,
    Texture as _,
};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use super::plymouth::Splash;
use super::splash::{self, Font, Images, Item};
use crate::backend::vulkan_scanout::{
    framebuffer_from_dmabuf, PrimeFramebuffer, VulkanScanoutAllocator, VulkanScanoutBuffer,
};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer, NATIVE_FOURCC};
use crate::ui::widget::{HAlign, Painter, ShapedText, TextShaper};

/// The outer's scanout buffers: one on screen, one to draw the next frame into. Flips go one at a
/// time, so a third would never be free when the other two are not.
const SLOTS: usize = 2;

#[derive(Clone)]
pub(super) enum Picture {
    Color([f32; 3]),
    Texture(VkTexture),
}

/// The outer's drawing device, shared by every display.
pub(super) struct Compositor {
    renderer: VulkanRenderer,
    node: Option<DrmNode>,
    /// The splash theme's images, loaded on first use.
    theme: Option<Theme>,
}

/// The splash theme's images, as textures. One that failed to load is left out of the splash.
struct Theme {
    watermark: Option<VkTexture>,
    spinner: Vec<VkTexture>,
}

impl Theme {
    fn load(renderer: &mut VulkanRenderer, dir: &Path) -> Self {
        let mut load = |name: &str| {
            let path = dir.join(name);
            match load_image(renderer, &path) {
                Ok(texture) => Some(texture),
                Err(err) => {
                    warn!("splash: {}: {err:#}", path.display());
                    None
                }
            }
        };
        let watermark = load("watermark.png");
        // All the frames or none: a spinner with a gap would stutter.
        let spinner = (1..)
            .map(|i| dir.join(format!("throbber-{i:04}.png")))
            .take_while(|path| path.exists())
            .map(|path| load_image(renderer, &path))
            .collect::<anyhow::Result<Vec<_>>>()
            .unwrap_or_else(|err| {
                warn!("splash: spinner: {err:#}");
                Vec::new()
            });
        Self { watermark, spinner }
    }

    fn images(&self) -> Images {
        let size = |t: &VkTexture| Size::from((t.width() as i32, t.height() as i32));
        Images {
            watermark: self.watermark.as_ref().map(size),
            spinner: self.spinner.first().map(|t| (size(t), self.spinner.len())),
        }
    }
}

/// A PNG as a premultiplied texture.
fn load_image(renderer: &mut VulkanRenderer, path: &Path) -> anyhow::Result<VkTexture> {
    let mut image = image::open(path)?.into_rgba8();
    for pixel in image.pixels_mut() {
        let [r, g, b, a] = pixel.0;
        let mul = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u8;
        pixel.0 = [mul(r), mul(g), mul(b), a];
    }
    let size = (image.width() as i32, image.height() as i32);
    renderer
        .import_memory(image.as_raw(), Fourcc::Abgr8888, size.into(), false)
        .map_err(|err| anyhow!("uploading: {err:?}"))
}

/// A splash laid out and its text shaped, ready to paint.
struct PreparedSplash {
    items: Vec<Item>,
    texts: Vec<ShapedText>,
    scale: f64,
}

/// One display's scanout buffers for the outer's frames, at that display's mode.
pub(super) struct Canvas {
    size: Size<i32, Physical>,
    slots: Vec<Slot>,
}

impl Canvas {
    pub fn new(size: (u16, u16)) -> Self {
        Self {
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            slots: Vec::new(),
        }
    }
}

struct Slot {
    _buffer: VulkanScanoutBuffer,
    dmabuf: Dmabuf,
    fb: Rc<PrimeFramebuffer>,
}

impl Compositor {
    pub fn new(render_node: (u64, u64)) -> anyhow::Result<Self> {
        let (major, minor) = render_node;
        let renderer = VulkanRenderer::for_drm_render_node(major as u32, minor as u32)
            .map_err(|err| anyhow!("bringing up Vulkan on {major}:{minor}: {err:?}"))?;
        let node = DrmNode::from_dev_id(libc::makedev(major as u32, minor as u32)).ok();
        Ok(Self {
            renderer,
            node,
            theme: None,
        })
    }

    /// A session frame as a texture. The caller guarantees its render is complete: the frame was
    /// on screen, or its fence has signalled.
    pub fn import(&mut self, dmabuf: &Dmabuf) -> anyhow::Result<VkTexture> {
        self.renderer
            .import_dmabuf(dmabuf, None)
            .map_err(|err| anyhow!("importing a session frame: {err:?}"))
    }

    /// Copy `dmabuf` into a texture the outer owns, so the picture outlives whatever its session
    /// does next — draw into that buffer again, take it back, or die (§5 step 1).
    pub fn snapshot(&mut self, dmabuf: &Dmabuf) -> anyhow::Result<VkTexture> {
        let source = Picture::Texture(self.import(dmabuf)?);
        let size = (dmabuf.width() as u16, dmabuf.height() as u16);
        self.capture(size, &source, &source, 0.)
    }

    /// `from` crossfaded towards `to` at `alpha`, drawn into a texture the outer owns, `size`
    /// pixels.
    pub fn capture(
        &mut self,
        size: (u16, u16),
        from: &Picture,
        to: &Picture,
        alpha: f32,
    ) -> anyhow::Result<VkTexture> {
        let size: Size<i32, Physical> = Size::from((i32::from(size.0), i32::from(size.1)));
        let mut target = self
            .renderer
            .create_buffer(
                NATIVE_FOURCC,
                size.to_logical(1).to_buffer(1, Transform::Normal),
            )
            .map_err(|err| anyhow!("allocating a snapshot: {err:?}"))?;
        {
            let mut fb = self
                .renderer
                .bind(&mut target)
                .map_err(|err| anyhow!("binding a snapshot: {err:?}"))?;
            let mut frame = self
                .renderer
                .render(&mut fb, size, Transform::Normal)
                .map_err(|err| anyhow!("starting a snapshot: {err:?}"))?;
            let full = Rectangle::from_size(size);
            draw(&mut frame, from, full, 1.0)?;
            draw(&mut frame, to, full, alpha)?;
            // Ordered before anything that samples it by the queue; nothing waits on it here.
            let _ = frame
                .finish()
                .map_err(|err| anyhow!("finishing a snapshot: {err:?}"))?;
        }
        Ok(target)
    }

    /// Draw `from` crossfaded towards `to` at `t` (0 is all `from`) into a free slot of `canvas`,
    /// avoiding `busy`. Returns the slot, its framebuffer and the fence that completes the draw.
    pub fn compose(
        &mut self,
        canvas: &mut Canvas,
        drm_fd: &DrmDeviceFd,
        busy: Option<usize>,
        from: &Picture,
        to: &Picture,
        t: f32,
    ) -> anyhow::Result<(usize, Rc<PrimeFramebuffer>, Option<OwnedFd>)> {
        if canvas.slots.is_empty() {
            self.allocate(canvas, drm_fd)?;
        }
        let index = (0..canvas.slots.len())
            .find(|&i| Some(i) != busy)
            .context("no free scanout slot")?;
        let size = canvas.size;
        let full = Rectangle::from_size(size);

        // The fence goes to the kernel as `IN_FENCE_FD`, so the submit may stay in flight.
        self.renderer.set_finish_may_defer(true);
        let result = (|| {
            let slot = &mut canvas.slots[index];
            let mut fb = self
                .renderer
                .bind(&mut slot.dmabuf)
                .map_err(|err| anyhow!("binding a scanout slot: {err:?}"))?;
            let mut frame = self
                .renderer
                .render(&mut fb, size, Transform::Normal)
                .map_err(|err| anyhow!("starting a frame: {err:?}"))?;
            draw(&mut frame, from, full, 1.0)?;
            draw(&mut frame, to, full, t)?;
            frame
                .finish()
                .map_err(|err| anyhow!("finishing a frame: {err:?}"))
        })();
        self.renderer.set_finish_may_defer(false);
        let sync = result?;

        let fence = sync.export();
        if fence.is_none() {
            sync.wait()
                .map_err(|err| anyhow!("waiting for a frame: {err:?}"))?;
        }
        Ok((index, canvas.slots[index].fb.clone(), fence))
    }

    /// Draw `splash` into a free slot of `canvas` (avoiding `busy`) at `scale`, its spinner at
    /// `spinner_frame`. Returns the slot, its framebuffer and the fence that completes the draw.
    #[expect(
        dead_code,
        reason = "the boot handover draws with it (boot-splash.md §5 step 3)"
    )]
    pub fn draw_splash(
        &mut self,
        canvas: &mut Canvas,
        drm_fd: &DrmDeviceFd,
        busy: Option<usize>,
        splash: &Splash,
        scale: f64,
        spinner_frame: usize,
    ) -> anyhow::Result<(usize, Rc<PrimeFramebuffer>, Option<OwnedFd>)> {
        if canvas.slots.is_empty() {
            self.allocate(canvas, drm_fd)?;
        }
        let index = (0..canvas.slots.len())
            .find(|&i| Some(i) != busy)
            .context("no free scanout slot")?;
        let size = canvas.size;
        let prepared = self.prepare_splash(splash, size, scale, spinner_frame);

        self.renderer.set_finish_may_defer(true);
        let result = (|| {
            let slot = &mut canvas.slots[index];
            let mut fb = self
                .renderer
                .bind(&mut slot.dmabuf)
                .map_err(|err| anyhow!("binding a scanout slot: {err:?}"))?;
            let mut frame = self
                .renderer
                .render(&mut fb, size, Transform::Normal)
                .map_err(|err| anyhow!("starting a frame: {err:?}"))?;
            paint_splash(&mut frame, size, &prepared, self.theme.as_ref())?;
            frame
                .finish()
                .map_err(|err| anyhow!("finishing a frame: {err:?}"))
        })();
        self.renderer.set_finish_may_defer(false);
        let sync = result?;

        let fence = sync.export();
        if fence.is_none() {
            sync.wait()
                .map_err(|err| anyhow!("waiting for a frame: {err:?}"))?;
        }
        Ok((index, canvas.slots[index].fb.clone(), fence))
    }

    /// `splash` drawn into a texture the outer owns, `size` pixels: the picture a crossfade
    /// starts or ends on.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the boot handover crossfades from it (boot-splash.md §5 step 3)"
        )
    )]
    pub fn splash_picture(
        &mut self,
        size: (u16, u16),
        splash: &Splash,
        scale: f64,
        spinner_frame: usize,
    ) -> anyhow::Result<VkTexture> {
        let size: Size<i32, Physical> = Size::from((i32::from(size.0), i32::from(size.1)));
        let prepared = self.prepare_splash(splash, size, scale, spinner_frame);
        let mut target = self
            .renderer
            .create_buffer(
                NATIVE_FOURCC,
                size.to_logical(1).to_buffer(1, Transform::Normal),
            )
            .map_err(|err| anyhow!("allocating a splash picture: {err:?}"))?;
        {
            let mut fb = self
                .renderer
                .bind(&mut target)
                .map_err(|err| anyhow!("binding a splash picture: {err:?}"))?;
            let mut frame = self
                .renderer
                .render(&mut fb, size, Transform::Normal)
                .map_err(|err| anyhow!("starting a splash picture: {err:?}"))?;
            paint_splash(&mut frame, size, &prepared, self.theme.as_ref())?;
            let _ = frame
                .finish()
                .map_err(|err| anyhow!("finishing a splash picture: {err:?}"))?;
        }
        Ok(target)
    }

    /// Lay out `splash` for a display of `size` physical pixels and shape its text. Never fails:
    /// what cannot be loaded or shaped is left out, as the splash must come up regardless.
    fn prepare_splash(
        &mut self,
        splash: &Splash,
        size: Size<i32, Physical>,
        scale: f64,
        spinner_frame: usize,
    ) -> PreparedSplash {
        let theme = self
            .theme
            .get_or_insert_with(|| Theme::load(&mut self.renderer, Path::new(splash::THEME_DIR)));
        let images = theme.images();
        let logical: Size<i32, Logical> = size.to_f64().to_logical(scale).to_i32_round();

        let mut shaper = TextShaper::new(&mut self.renderer, scale);
        let mut shaped = Vec::new();
        let mut measure = |text: &str, font: Font| match shaper.shape_named(
            text,
            font.px(),
            splash::FAMILY,
            font.weight(),
        ) {
            Ok(run) => {
                let (_, _, ink_w, _) = run.ink_bounds();
                let size = Size::from((
                    (f64::from(ink_w) / scale).round() as i32,
                    (f64::from(run.line_box_height()) / scale).round() as i32,
                ));
                shaped.push((text.to_owned(), font, Some(run)));
                size
            }
            Err(err) => {
                warn!("splash: shaping {text:?}: {err:#}");
                shaped.push((text.to_owned(), font, None));
                Size::from((0, 0))
            }
        };
        let laid = splash::layout(splash, logical, &images, spinner_frame, &mut measure);

        // Each text item was measured exactly once, in order; keep the ones that shaped.
        let mut items = Vec::with_capacity(laid.len());
        let mut texts = Vec::new();
        let mut shaped = shaped.into_iter();
        for item in laid {
            if let Item::Text { text, font, .. } = &item {
                let run = shaped
                    .find(|(t, f, _)| t == text && f == font)
                    .and_then(|(_, _, run)| run);
                let Some(run) = run else { continue };
                texts.push(run);
            }
            items.push(item);
        }
        PreparedSplash {
            items,
            texts,
            scale,
        }
    }

    fn allocate(&mut self, canvas: &mut Canvas, drm_fd: &DrmDeviceFd) -> anyhow::Result<()> {
        let mut allocator = VulkanScanoutAllocator::new(self.renderer.gpu().clone(), self.node)?;
        for _ in 0..SLOTS {
            // The format and layout a session's own scanout buffers use (`backend::nested`).
            let buffer = allocator
                .create_buffer(
                    canvas.size.w as u32,
                    canvas.size.h as u32,
                    Fourcc::Xrgb8888,
                    &[Modifier::Linear],
                )
                .map_err(|err| anyhow!("{err}"))?;
            let dmabuf = buffer.export().context("exporting a scanout slot")?;
            let fb = framebuffer_from_dmabuf(drm_fd, &dmabuf, true)
                .map_err(|err| anyhow!("adding a scanout slot framebuffer: {err}"))?;
            canvas.slots.push(Slot {
                _buffer: buffer,
                dmabuf,
                fb: Rc::new(fb),
            });
        }
        Ok(())
    }
}

/// Paint a prepared splash over black.
fn paint_splash(
    frame: &mut VulkanFrame<'_, '_>,
    size: Size<i32, Physical>,
    prepared: &PreparedSplash,
    theme: Option<&Theme>,
) -> anyhow::Result<()> {
    let mut painter = Painter::new(frame, prepared.scale, size);
    painter.clear([0., 0., 0., 1.])?;
    let logical = |at: Point<i32, Logical>, size: Size<i32, Logical>| {
        Rectangle::new(at.to_f64(), size.to_f64())
    };
    let size_of = |t: &VkTexture| Size::from((t.width() as i32, t.height() as i32));
    let mut texts = prepared.texts.iter();
    for item in &prepared.items {
        match item {
            Item::Watermark(at) => {
                if let Some(t) = theme.and_then(|t| t.watermark.as_ref()) {
                    painter.image(t, logical(*at, size_of(t)), 1.0)?;
                }
            }
            Item::Spinner { frame, at } => {
                if let Some(t) = theme.and_then(|t| t.spinner.get(*frame)) {
                    painter.image(t, logical(*at, size_of(t)), 1.0)?;
                }
            }
            Item::Bar { rect, filled } => {
                let [r, g, b] = splash::BAR_BACKGROUND;
                let full = logical(rect.loc, rect.size);
                painter.fill_rounded(full, 0., [r, g, b, 1.])?;
                if *filled > 0 {
                    let [r, g, b] = splash::BAR_FOREGROUND;
                    let done = logical(rect.loc, Size::from((*filled, rect.size.h)));
                    painter.fill_rounded(done, 0., [r, g, b, 1.])?;
                }
            }
            Item::Text { at, .. } => {
                let Some(run) = texts.next() else { continue };
                let band = f64::from(run.line_box_height()) / prepared.scale;
                let full = Rectangle::from_size(size.to_f64().to_logical(prepared.scale));
                painter.text_band(
                    run,
                    f64::from(at.x),
                    HAlign::Left,
                    f64::from(at.y),
                    band,
                    [1., 1., 1., 1.],
                    full,
                )?;
            }
        }
    }
    Ok(())
}

/// Draw `picture` over the whole frame at opacity `alpha`, blending over what is there.
fn draw(
    frame: &mut <VulkanRenderer as smithay::backend::renderer::RendererSuper>::Frame<'_, '_>,
    picture: &Picture,
    full: Rectangle<i32, Physical>,
    alpha: f32,
) -> anyhow::Result<()> {
    if alpha <= 0.0 {
        return Ok(());
    }
    match picture {
        Picture::Color([r, g, b]) => frame
            .draw_solid(
                full,
                &[full],
                Color32F::new(r * alpha, g * alpha, b * alpha, alpha),
            )
            .map_err(|err| anyhow!("drawing a colour: {err:?}")),
        Picture::Texture(texture) => {
            let src: Rectangle<f64, BufferCoord> = Rectangle::from_size(
                (f64::from(texture.width()), f64::from(texture.height())).into(),
            );
            frame
                .render_texture_from_to(texture, src, full, &[full], &[], Transform::Normal, alpha)
                .map_err(|err| anyhow!("drawing a texture: {err:?}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::ExportMem as _;

    use super::*;
    use crate::system_compositor::plymouth::Mode;

    fn compositor() -> Option<Compositor> {
        match VulkanRenderer::new() {
            Ok(renderer) => Some(Compositor {
                renderer,
                node: None,
                theme: None,
            }),
            Err(err) => {
                eprintln!("skipping: no Vulkan device ({err})");
                None
            }
        }
    }

    /// `texture`'s pixels, RGBA.
    fn pixels(c: &mut Compositor, texture: &mut VkTexture) -> (i32, Vec<u8>) {
        let size = texture.size();
        let fb = c.renderer.bind(texture).unwrap();
        let mapping = c
            .renderer
            .copy_framebuffer(&fb, Rectangle::from_size(size), Fourcc::Abgr8888)
            .unwrap();
        let bytes = c.renderer.map_texture(&mapping).unwrap().to_vec();
        (size.w, bytes)
    }

    /// The brightest pixel in `rect`.
    fn brightest((width, bytes): &(i32, Vec<u8>), rect: Rectangle<i32, Physical>) -> u8 {
        let mut max = 0;
        for y in rect.loc.y..rect.loc.y + rect.size.h {
            for x in rect.loc.x..rect.loc.x + rect.size.w {
                let i = ((y * width + x) * 4) as usize;
                max = max.max(bytes[i]).max(bytes[i + 1]).max(bytes[i + 2]);
            }
        }
        max
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// The installed theme, drawn where the layout says: the watermark, the spinner, the
    /// progress bar's two colours and a message's ink, on black everywhere else.
    #[test]
    fn the_splash_draws_the_theme() {
        if !Path::new(splash::THEME_DIR).join("watermark.png").exists() {
            eprintln!("skipping: no plymouth spinner theme");
            return;
        }
        let Some(mut c) = compositor() else { return };

        let boot = Splash {
            shown: true,
            messages: vec!["Hello".into()],
            ..Splash::default()
        };
        let mut texture = c.splash_picture((1280, 800), &boot, 1., 0).unwrap();
        let px = pixels(&mut c, &mut texture);
        // The watermark at 565×726, 149×43; the spinner at 624×544, 32×32.
        assert!(brightest(&px, rect(565, 726, 149, 43)) > 128);
        assert!(brightest(&px, rect(624, 544, 32, 32)) > 64);
        // The message, 10 px under the spinner.
        assert!(brightest(&px, rect(560, 586, 160, 24)) > 128);
        // Black elsewhere.
        assert_eq!(brightest(&px, rect(0, 0, 1280, 500)), 0);

        let mut update = Splash {
            mode: Mode::Updates,
            shown: true,
            progress: Some(50),
            ..Splash::default()
        };
        let mut texture = c.splash_picture((1280, 800), &update, 1., 0).unwrap();
        let px = pixels(&mut c, &mut texture);
        // The bar at 440×556, 400×5: white to half, grey after.
        let (width, bytes) = &px;
        let at = |x: i32, y: i32| bytes[((y * width + x) * 4) as usize];
        assert_eq!(at(540, 558), 255);
        assert_eq!(at(740, 558), 0x60);
        // The title, near 0.382 of the height, and no spinner.
        assert!(brightest(&px, rect(400, 250, 480, 60)) > 128);
        assert_eq!(brightest(&px, rect(624, 520, 32, 30)), 0);

        // The retained frame: the watermark alone.
        update.shown = false;
        let mut texture = c.splash_picture((1280, 800), &update, 1., 0).unwrap();
        let px = pixels(&mut c, &mut texture);
        assert!(brightest(&px, rect(565, 726, 149, 43)) > 128);
        assert_eq!(brightest(&px, rect(0, 0, 1280, 720)), 0);
    }
}
