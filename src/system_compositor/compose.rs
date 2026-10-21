// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The outer's own drawing: what it composites during a session switch (§5), and the boot splash
//! (`docs/fork/boot-splash.md` §4).
//!
//! A switch is two pictures, moved or crossfaded ([`Motion`]) into one of the outer's own scanout
//! buffers. A picture is either a flat colour (the plate) or a texture: a snapshot the outer took
//! of what was on screen, or a session frame imported from its dmabuf. Steady state never comes
//! here — a fronted session's buffers are flipped as they are.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Context as _};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::{Allocator as _, Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::{DrmDeviceFd, DrmNode};
use smithay::backend::renderer::{
    Bind as _, Color32F, Frame as _, ImportDma as _, ImportMem as _, Offscreen as _, Renderer as _,
    Texture as _,
};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Rectangle, Size, Transform};
use synoik_splash::bgrt::{self, Logo};
use synoik_splash::layout as geo;

use super::plymouth::Splash;
use super::splash::{self, Dialog, Font, Images, Item};
use crate::backend::vulkan_scanout::{
    framebuffer_from_dmabuf, PrimeFramebuffer, VulkanScanoutAllocator, VulkanScanoutBuffer,
};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer, NATIVE_FOURCC};
use crate::ui::widget::{HAlign, Painter, ShapedText, TextShaper};

/// GNOME's dark plate (`$system_bg_color`, #242424 in libadwaita dark), the colour a session's
/// own background sits on.
pub(super) const PLATE: [u8; 3] = [0x24, 0x24, 0x24];

/// The outer's scanout buffers: one on screen, one to draw the next frame into. Flips go one at a
/// time, so a third would never be free when the other two are not.
const SLOTS: usize = 2;

#[derive(Clone)]
pub(super) enum Picture {
    Color([f32; 3]),
    Texture(VkTexture),
}

/// How a switch between sessions looks (`SYNOIK_SYSTEM_TRANSITION`). Sessions are told it
/// (`ToInner::Welcome`), so what they bring up for a switch moves the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Style {
    /// The outgoing picture crossfades into the incoming one.
    Fade,
    /// The greeter is a curtain over the sessions, as gnome-shell's lock screen is over a
    /// session (`screenShield.js`): it slides up off the session to let it in, and down over it to
    /// close it.
    Curtain,
    /// The greeter comes towards the viewer and fades as the session lands from a distance, as
    /// gnome-shell's session starts (`_startupAnimationSession`, `layout.js`); backwards, the
    /// session recedes and the greeter settles in front.
    Depth,
}

impl Style {
    /// `fade`, `curtain` or `depth`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fade" => Some(Self::Fade),
            "curtain" => Some(Self::Curtain),
            "depth" => Some(Self::Depth),
            _ => None,
        }
    }
}

/// Which way a switch goes: the greeter is the front picture, the one that moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Way {
    /// The front picture leaves, the back one comes in: a login, an unlock (greeter to session).
    Leaves,
    /// The front picture comes in over the back one: a logout, a switch user (session to
    /// greeter).
    Arrives,
    /// From one user's session to another's, with no greeter between: the outgoing session is the
    /// front picture, leaving as a greeter would; a curtain is the plate, down over the outgoing
    /// session and up off the incoming one in one motion.
    Through,
}

/// How a switch moves its two pictures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Motion {
    pub style: Style,
    pub way: Way,
    /// Behind both pictures, and the curtain of a user-to-user switch: the plate between
    /// sessions, black between two screens of the splash.
    pub backdrop: [u8; 3],
}

/// The depth a session lands from and recedes to: gnome-shell's session start begins at this
/// scale (`_prepareStartupAnimation`, `layout.js`).
const DEPTH_FAR: f64 = 0.75;
/// How near the front picture comes as it leaves: it flies past, as if gone through and left
/// behind. As it fades out of sight (under 5% at 78% of the motion) about a tenth of its width
/// is still on screen. Ours: gnome-shell has no greeter moving out.
pub const DEPTH_NEAR: f64 = 12.;

impl Motion {
    pub const FADE: Self = Self::new(Style::Fade, Way::Leaves);

    /// A motion over the plate.
    pub const fn new(style: Style, way: Way) -> Self {
        Self {
            style,
            way,
            backdrop: PLATE,
        }
    }

    /// How long it runs: the crossfade's 250 ms; the curtain's slide (`CURTAIN_SLIDE_TIME`,
    /// `screenShield.js`), twice between two users; the depth as long as one slide (ours: kov's
    /// pick, where gnome-shell's session start takes 500 ms, `STARTUP_ANIMATION_TIME`).
    pub fn duration(self) -> Duration {
        Duration::from_millis(match (self.style, self.way) {
            (Style::Fade, _) => 250,
            (Style::Curtain, Way::Through) => 600,
            (Style::Curtain, _) => 300,
            (Style::Depth, _) => 300,
        })
    }

    /// Progress at `t` of the duration, eased out: the crossfade cubic, the motions quadratic as
    /// gnome-shell's (`EASE_OUT_QUAD`). A curtain between two users is two slides, down and up,
    /// each eased on its own: progress 0.5 is the curtain down, at half the duration.
    pub fn ease(self, t: f32) -> f32 {
        let t = t.clamp(0., 1.);
        let quad = |t: f32| 1. - (1. - t).powi(2);
        match (self.style, self.way) {
            (Style::Fade, _) => 1. - (1. - t).powi(3),
            (Style::Curtain, Way::Through) if t < 0.5 => quad(t * 2.) / 2.,
            (Style::Curtain, Way::Through) => 0.5 + quad(t * 2. - 1.) / 2.,
            (Style::Curtain | Style::Depth, _) => quad(t),
        }
    }
}

impl std::fmt::Display for Motion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let style = match self.style {
            Style::Fade => "fade",
            Style::Curtain => "curtain",
            Style::Depth => "depth",
        };
        let way = match self.way {
            Way::Leaves => "leaves",
            Way::Arrives => "arrives",
            Way::Through => "through",
        };
        write!(f, "{style} {way}")
    }
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
    /// The firmware's logo, and where the firmware drew it (`synoik_splash::bgrt`).
    firmware: Option<(VkTexture, geo::Point)>,
    watermark: Option<VkTexture>,
    spinner: Vec<VkTexture>,
    /// The password dialog's.
    lock: Option<VkTexture>,
    entry: Option<VkTexture>,
    bullet: Option<VkTexture>,
    keyboard: Option<VkTexture>,
    caps_lock: Option<VkTexture>,
}

impl Theme {
    /// The theme in `dir`, and the firmware's logo from the table in `bgrt`.
    fn load(renderer: &mut VulkanRenderer, dir: &Path, bgrt: &Path) -> Self {
        let firmware = Logo::load(bgrt).and_then(|logo| {
            let bytes: Vec<u8> = logo
                .image
                .pixels
                .iter()
                .flat_map(|p| {
                    let [b, g, r, a] = p.to_le_bytes();
                    [r, g, b, a]
                })
                .collect();
            let size = (logo.image.size.w, logo.image.size.h);
            match renderer.import_memory(&bytes, Fourcc::Abgr8888, size.into(), false) {
                Ok(texture) => Some((texture, logo.offset)),
                Err(err) => {
                    warn!("splash: firmware logo: uploading: {err:?}");
                    None
                }
            }
        });
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
        let watermark = match load_image(renderer, &geo::watermark_file(dir)) {
            Ok(texture) => Some(texture),
            Err(err) => {
                warn!("splash: watermark: {err:#}");
                None
            }
        };
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
        Self {
            firmware,
            watermark,
            spinner,
            lock: load("lock.png"),
            entry: load("entry.png"),
            bullet: load("bullet.png"),
            keyboard: load("keyboard.png"),
            caps_lock: load("capslock.png"),
        }
    }

    /// The images' sizes, the firmware's logo placed for `panel` ([`splash::panel`]): logical
    /// pixels at `scale`, the panel's taken as the display's.
    fn images(&self, panel: geo::Size, scale: f64) -> Images {
        let size = |t: &VkTexture| geo::Size::from((t.width() as i32, t.height() as i32));
        let logical = |v: i32| (f64::from(v) / scale).round() as i32;
        Images {
            firmware: self.firmware.as_ref().map(|(t, offset)| {
                let t = size(t);
                geo::Firmware {
                    size: geo::Size::from((logical(t.w), logical(t.h))),
                    offset: geo::Point::from((logical(offset.x), logical(offset.y))),
                    panel: geo::Size::from((logical(panel.w), logical(panel.h))),
                }
            }),
            watermark: self.watermark.as_ref().map(size),
            spinner: self.spinner.first().map(|t| (size(t), self.spinner.len())),
            lock: self.lock.as_ref().map(size),
            entry: self.entry.as_ref().map(size),
            bullet: self.bullet.as_ref().map(size),
            keyboard: self.keyboard.as_ref().map(size),
            caps_lock: self.caps_lock.as_ref().map(size),
            cell: None,
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
    /// One per [`Item::Text`] and [`Item::Console`] in `items`, in order.
    texts: Vec<ShapedText>,
    scale: f64,
    /// The details view's cell, logical: its lines are centred in it.
    cell: Option<geo::Size>,
}

/// The details view's character cell (§8): a monospace face's advance, which two runs of one
/// character apart differ by, and its line box; logical, at `scale`. `None` without the face.
fn mono_cell(shaper: &mut TextShaper<'_>, scale: f64) -> Option<geo::Size> {
    let font = Font::Mono { bold: false };
    let mut shape = |text: &str| {
        shaper
            .shape_named(text, font.px(), font.family(), font.weight())
            .ok()
    };
    let (one, two) = (shape("M")?, shape("MM")?);
    let advance = two.ink_bounds().2 - one.ink_bounds().2;
    let height = one.line_box_height();
    let logical = |px: i32| (f64::from(px) / scale).round() as i32;
    (advance > 0 && height > 0).then(|| geo::Size::from((logical(advance), logical(height))))
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
        self.capture(size, &source, &source, Motion::FADE, 0.)
    }

    /// `from` on its way to `to` by `motion`, `progress` of the way there, drawn into a texture
    /// the outer owns, `size` pixels: what a transition shows, as [`Self::compose`] draws it.
    pub fn capture(
        &mut self,
        size: (u16, u16),
        from: &Picture,
        to: &Picture,
        motion: Motion,
        progress: f32,
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
            paint(&mut frame, size, from, to, motion, progress)?;
            // Ordered before anything that samples it by the queue; nothing waits on it here.
            let _ = frame
                .finish()
                .map_err(|err| anyhow!("finishing a snapshot: {err:?}"))?;
        }
        Ok(target)
    }

    /// Draw `from` on its way to `to` by `motion`, `progress` of the way there (0 is all `from`),
    /// into a free slot of `canvas`, avoiding `busy`. Returns the slot, its framebuffer and the
    /// fence that completes the draw.
    #[allow(clippy::too_many_arguments)]
    pub fn compose(
        &mut self,
        canvas: &mut Canvas,
        drm_fd: &DrmDeviceFd,
        busy: Option<usize>,
        from: &Picture,
        to: &Picture,
        motion: Motion,
        progress: f32,
    ) -> anyhow::Result<(usize, Rc<PrimeFramebuffer>, Option<OwnedFd>)> {
        if canvas.slots.is_empty() {
            self.allocate(canvas, drm_fd)?;
        }
        let index = (0..canvas.slots.len())
            .find(|&i| Some(i) != busy)
            .context("no free scanout slot")?;
        let size = canvas.size;

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
            paint(&mut frame, size, from, to, motion, progress)?;
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
    /// `spinner_frame`, a password being typed as `dialog` says, the firmware's logo placed for a
    /// `panel` of that mode ([`splash::panel`]). Returns the slot, its framebuffer and the fence
    /// that completes the draw.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_splash(
        &mut self,
        canvas: &mut Canvas,
        drm_fd: &DrmDeviceFd,
        busy: Option<usize>,
        splash: &Splash,
        scale: f64,
        panel: (u16, u16),
        spinner_frame: usize,
        dialog: &Dialog,
    ) -> anyhow::Result<(usize, Rc<PrimeFramebuffer>, Option<OwnedFd>)> {
        if canvas.slots.is_empty() {
            self.allocate(canvas, drm_fd)?;
        }
        let index = (0..canvas.slots.len())
            .find(|&i| Some(i) != busy)
            .context("no free scanout slot")?;
        let size = canvas.size;
        let prepared = self.prepare_splash(splash, size, scale, panel, spinner_frame, dialog);

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

    /// How many frames the theme's spinner has; 0 without one.
    pub fn spinner_frames(&mut self) -> usize {
        self.theme().spinner.len()
    }

    fn theme(&mut self) -> &Theme {
        self.theme.get_or_insert_with(|| {
            let (dir, bgrt) = (Path::new(splash::THEME_DIR), Path::new(bgrt::DIR));
            Theme::load(&mut self.renderer, dir, bgrt)
        })
    }

    /// `splash` drawn into a texture the outer owns, `size` pixels: the picture a crossfade
    /// starts or ends on. Drawn into `reuse` when that is a picture of the same size, so a splash
    /// redrawn as its spinner turns allocates nothing. Queue order keeps the redraw after every
    /// blend already submitted that reads it. `panel` as for [`Self::draw_splash`].
    #[allow(clippy::too_many_arguments)]
    pub fn splash_picture(
        &mut self,
        size: (u16, u16),
        splash: &Splash,
        scale: f64,
        panel: (u16, u16),
        spinner_frame: usize,
        dialog: &Dialog,
        reuse: Option<VkTexture>,
    ) -> anyhow::Result<VkTexture> {
        let size: Size<i32, Physical> = Size::from((i32::from(size.0), i32::from(size.1)));
        let prepared = self.prepare_splash(splash, size, scale, panel, spinner_frame, dialog);
        let reuse = reuse.filter(|t| (t.width() as i32, t.height() as i32) == (size.w, size.h));
        let mut target = match reuse {
            Some(texture) => texture,
            None => self
                .renderer
                .create_buffer(
                    NATIVE_FOURCC,
                    size.to_logical(1).to_buffer(1, Transform::Normal),
                )
                .map_err(|err| anyhow!("allocating a splash picture: {err:?}"))?,
        };
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
        panel: (u16, u16),
        spinner_frame: usize,
        dialog: &Dialog,
    ) -> PreparedSplash {
        let panel = geo::Size::from((i32::from(panel.0), i32::from(panel.1)));
        let mut images = self.theme().images(panel, scale);
        let logical: Size<i32, Logical> = size.to_f64().to_logical(scale).to_i32_round();
        let logical = geo::Size::from((logical.w, logical.h));

        let mut shaper = TextShaper::new(&mut self.renderer, scale);
        if splash.details {
            images.cell = mono_cell(&mut shaper, scale);
        }
        let mut shaped = Vec::new();
        let mut measure = |text: &str, font: Font| match shaper.shape_named(
            text,
            font.px(),
            font.family(),
            font.weight(),
        ) {
            Ok(run) => {
                let (_, _, ink_w, _) = run.ink_bounds();
                let size = geo::Size::from((
                    (f64::from(ink_w) / scale).round() as i32,
                    (f64::from(run.line_box_height()) / scale).round() as i32,
                ));
                shaped.push((text.to_owned(), font, Some(run)));
                size
            }
            Err(err) => {
                warn!("splash: shaping {text:?}: {err:#}");
                shaped.push((text.to_owned(), font, None));
                geo::Size::default()
            }
        };
        let laid = splash::layout(
            splash,
            logical,
            &images,
            spinner_frame,
            dialog,
            &mut measure,
        );

        // Each text item was measured exactly once, in order; keep the ones that shaped. The
        // details view's runs are not measured (its cell places them): shape them here.
        let mut items = Vec::with_capacity(laid.len());
        let mut texts = Vec::new();
        let mut shaped = shaped.into_iter();
        for item in laid {
            match &item {
                Item::Text { text, font, .. } => {
                    let run = shaped
                        .find(|(t, f, _)| t == text && f == font)
                        .and_then(|(_, _, run)| run);
                    let Some(run) = run else { continue };
                    texts.push(run);
                }
                Item::Console(placed) => {
                    let font = Font::Mono {
                        bold: placed.style.bold,
                    };
                    match shaper.shape_named(&placed.text, font.px(), font.family(), font.weight())
                    {
                        Ok(run) => texts.push(run),
                        Err(err) => {
                            warn!("splash: shaping {:?}: {err:#}", placed.text);
                            continue;
                        }
                    }
                }
                _ => (),
            }
            items.push(item);
        }
        PreparedSplash {
            items,
            texts,
            scale,
            cell: images.cell,
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
    let logical = |at: geo::Point, size: geo::Size| {
        Rectangle::<f64, Logical>::new(
            (f64::from(at.x), f64::from(at.y)).into(),
            (f64::from(size.w), f64::from(size.h)).into(),
        )
    };
    let size_of = |t: &VkTexture| geo::Size::from((t.width() as i32, t.height() as i32));
    let mut texts = prepared.texts.iter();
    for item in &prepared.items {
        match item {
            Item::Firmware(at) => {
                if let Some((t, _)) = theme.and_then(|t| t.firmware.as_ref()) {
                    // At the scale the layout placed it: its pixels are the firmware's.
                    let size = geo::Size::from((
                        (f64::from(t.width() as i32) / prepared.scale).round() as i32,
                        (f64::from(t.height() as i32) / prepared.scale).round() as i32,
                    ));
                    painter.image(t, logical(*at, size), 1.0)?;
                }
            }
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
            Item::Lock(at) | Item::Entry(at) | Item::Keyboard(at) | Item::CapsLock(at) => {
                let texture = theme.and_then(|t| match item {
                    Item::Lock(_) => t.lock.as_ref(),
                    Item::Entry(_) => t.entry.as_ref(),
                    Item::Keyboard(_) => t.keyboard.as_ref(),
                    _ => t.caps_lock.as_ref(),
                });
                if let Some(t) = texture {
                    painter.image(t, logical(*at, size_of(t)), 1.0)?;
                }
            }
            Item::Bullet { at, src } => {
                if let Some(t) = theme.and_then(|t| t.bullet.as_ref()) {
                    let from = Rectangle::new(
                        (f64::from(src.loc.x), f64::from(src.loc.y)).into(),
                        (f64::from(src.size.w), f64::from(src.size.h)).into(),
                    );
                    painter.image_region(t, from, logical(*at, src.size), 1.0)?;
                }
            }
            Item::Bar { rect, filled } => {
                let [r, g, b] = splash::BAR_BACKGROUND;
                let full = logical(rect.loc, rect.size);
                painter.fill_rounded(full, 0., [r, g, b, 1.])?;
                if *filled > 0 {
                    let [r, g, b] = splash::BAR_FOREGROUND;
                    let done = logical(rect.loc, geo::Size::from((*filled, rect.size.h)));
                    painter.fill_rounded(done, 0., [r, g, b, 1.])?;
                }
            }
            Item::Console(placed) => {
                let Some(run) = texts.next() else { continue };
                // The pen, not the ink, at the column: the runs of a line share one grid.
                let band = prepared.cell.map_or(run.line_box_height(), |c| {
                    (f64::from(c.h) * prepared.scale) as i32
                });
                let px = |v: i32| (f64::from(v) * prepared.scale).round() as i32;
                let origin = smithay::utils::Point::<i32, Physical>::from((
                    px(placed.at.x),
                    px(placed.at.y) + run.line_box_centered_y(band),
                ));
                let [r, g, b] = placed.style.color.rgb();
                painter.text_px(run, origin, [r, g, b, 1.])?;
            }
            Item::Text { at, font, .. } => {
                let Some(run) = texts.next() else { continue };
                let band = f64::from(run.line_box_height()) / prepared.scale;
                let full = Rectangle::from_size(size.to_f64().to_logical(prepared.scale));
                painter.text_band(
                    run,
                    f64::from(at.x),
                    HAlign::Left,
                    f64::from(at.y),
                    band,
                    {
                        let [r, g, b] = font.color();
                        [r, g, b, 1.]
                    },
                    full,
                )?;
            }
        }
    }
    Ok(())
}

type OuterFrame<'a, 'b> =
    <VulkanRenderer as smithay::backend::renderer::RendererSuper>::Frame<'a, 'b>;

/// One of a transition's pictures.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layer {
    From,
    To,
    /// The plate: behind every motion, and the curtain of a user-to-user one.
    Plate,
}

/// Where each picture of a transition goes and how opaque it is, `progress` of the way from
/// `from` to `to`, in drawing order. The one place a motion's geometry lives, so a snapshot taken
/// mid-transition is what was on screen.
fn layers(
    size: Size<i32, Physical>,
    motion: Motion,
    progress: f32,
) -> Vec<(Layer, Rectangle<i32, Physical>, f32)> {
    let full = Rectangle::from_size(size);
    let p = f64::from(progress.clamp(0., 1.));
    if motion.style == Style::Fade {
        return vec![(Layer::From, full, 1.), (Layer::To, full, progress)];
    }
    // `front` is the greeter's side, `cover` how much of the way it is in: 1 on screen, 0 gone.
    let (front, back, cover) = match motion.way {
        Way::Leaves => (Layer::From, Layer::To, 1. - p),
        Way::Arrives => (Layer::To, Layer::From, p),
        Way::Through if motion.style == Style::Depth => (Layer::From, Layer::To, 1. - p),
        // The plate's curtain, down over `from` for the first half and up off `to` for the second.
        Way::Through if p < 0.5 => (Layer::Plate, Layer::From, p * 2.),
        Way::Through => (Layer::Plate, Layer::To, (1. - p) * 2.),
    };
    let mut layers = vec![(Layer::Plate, full, 1.)];
    match motion.style {
        Style::Fade => unreachable!("handled above"),
        Style::Curtain => {
            let y = -((1. - cover) * f64::from(size.h)).round() as i32;
            layers.push((back, full, 1.));
            layers.push((front, Rectangle::new((0, y).into(), size), 1.));
        }
        Style::Depth => {
            let back_scale = DEPTH_FAR + (1. - DEPTH_FAR) * (1. - cover);
            let front_scale = 1. + (DEPTH_NEAR - 1.) * (1. - cover);
            layers.push((back, scaled(size, back_scale), (1. - cover) as f32));
            // Faster out than it grows, so it is mostly gone before it is much past the screen.
            layers.push((front, scaled(size, front_scale), (cover * cover) as f32));
        }
    }
    layers
}

/// The frame's rectangle scaled by `scale` about its centre.
fn scaled(size: Size<i32, Physical>, scale: f64) -> Rectangle<i32, Physical> {
    let w = (f64::from(size.w) * scale).round() as i32;
    let h = (f64::from(size.h) * scale).round() as i32;
    Rectangle::new(((size.w - w) / 2, (size.h - h) / 2).into(), (w, h).into())
}

/// Draw a transition's frame: its [`layers`].
fn paint(
    frame: &mut OuterFrame<'_, '_>,
    size: Size<i32, Physical>,
    from: &Picture,
    to: &Picture,
    motion: Motion,
    progress: f32,
) -> anyhow::Result<()> {
    let full = Rectangle::from_size(size);
    let plate = Picture::Color(motion.backdrop.map(|c| f32::from(c) / 255.));
    for (layer, dst, alpha) in layers(size, motion, progress) {
        let picture = match layer {
            Layer::From => from,
            Layer::To => to,
            Layer::Plate => &plate,
        };
        draw(frame, picture, dst, full, alpha)?;
    }
    Ok(())
}

/// Draw `picture` into `dst` (which may reach past the frame, `full`) at opacity `alpha`, blending
/// over what is there.
fn draw(
    frame: &mut OuterFrame<'_, '_>,
    picture: &Picture,
    dst: Rectangle<i32, Physical>,
    full: Rectangle<i32, Physical>,
    alpha: f32,
) -> anyhow::Result<()> {
    // The part of `dst` on the frame, as damage local to `dst`.
    let Some(visible) = dst.intersection(full) else {
        return Ok(());
    };
    if alpha <= 0.0 {
        return Ok(());
    }
    let damage = [Rectangle::new(visible.loc - dst.loc, visible.size)];
    match picture {
        Picture::Color([r, g, b]) => frame
            .draw_solid(
                dst,
                &damage,
                Color32F::new(r * alpha, g * alpha, b * alpha, alpha),
            )
            .map_err(|err| anyhow!("drawing a colour: {err:?}")),
        Picture::Texture(texture) => {
            let src: Rectangle<f64, BufferCoord> = Rectangle::from_size(
                (f64::from(texture.width()), f64::from(texture.height())).into(),
            );
            frame
                .render_texture_from_to(texture, src, dst, &damage, &[], Transform::Normal, alpha)
                .map_err(|err| anyhow!("drawing a texture: {err:?}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::ExportMem as _;

    use super::*;
    use crate::system_compositor::plymouth::Mode;

    /// A compositor whose theme has no firmware logo, whatever the machine running the tests
    /// publishes: a test wanting one loads it from a table of its own (`bgrt_table`).
    fn compositor() -> Option<Compositor> {
        match VulkanRenderer::new() {
            Ok(mut renderer) => {
                let theme = Theme::load(
                    &mut renderer,
                    Path::new(splash::THEME_DIR),
                    Path::new("/nonexistent"),
                );
                Some(Compositor {
                    renderer,
                    node: None,
                    theme: Some(theme),
                })
            }
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

    const RED: Picture = Picture::Color([1., 0., 0.]);
    const BLUE: Picture = Picture::Color([0., 0., 1.]);

    fn every_motion() -> Vec<Motion> {
        let mut motions = Vec::new();
        for style in [Style::Fade, Style::Curtain, Style::Depth] {
            for way in [Way::Leaves, Way::Arrives, Way::Through] {
                motions.push(Motion::new(style, way));
            }
        }
        motions
    }

    /// A transition `progress` of the way from red to blue, 64×40, as RGB at `(x, y)`.
    fn transition(
        c: &mut Compositor,
        motion: Motion,
        progress: f32,
    ) -> impl Fn(i32, i32) -> [u8; 3] {
        let mut texture = c.capture((64, 40), &RED, &BLUE, motion, progress).unwrap();
        let (width, bytes) = pixels(c, &mut texture);
        move |x, y| {
            let i = ((y * width + x) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2]]
        }
    }

    fn near(got: [u8; 3], want: [u8; 3]) -> bool {
        got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 3)
    }

    /// Every motion starts on its `from` picture and ends on its `to`, whole, so neither end of a
    /// switch jumps.
    #[test]
    fn a_transition_starts_on_from_and_ends_on_to() {
        let Some(mut c) = compositor() else { return };
        for motion in every_motion() {
            for (progress, want) in [(0., [255, 0, 0]), (1., [0, 0, 255])] {
                let at = transition(&mut c, motion, progress);
                for (x, y) in [(0, 0), (63, 0), (32, 20), (0, 39), (63, 39)] {
                    assert!(
                        near(at(x, y), want),
                        "{motion} at {progress}: {:?} at {x},{y}",
                        at(x, y)
                    );
                }
            }
        }
    }

    /// A curtain between two users is down at half its duration, each slide eased on its own.
    #[test]
    fn a_curtain_between_users_is_two_slides() {
        let through = Motion::new(Style::Curtain, Way::Through);
        assert_eq!(through.ease(0.5), 0.5);
        assert!(
            through.ease(0.25) > 0.25 && through.ease(0.75) > 0.75,
            "both eased out"
        );
        assert_eq!(
            through.duration(),
            2 * Motion {
                way: Way::Leaves,
                ..through
            }
            .duration()
        );
    }

    /// Halfway, a curtain is half down: the greeter's side above, the session below. Leaving, the
    /// greeter is `from`; arriving, `to`; between two users the plate is the curtain, over
    /// `from` and then off `to`.
    #[test]
    fn a_curtain_slides_from_the_top() {
        let Some(mut c) = compositor() else { return };
        let plate = PLATE;
        let cases = [
            (Way::Leaves, 0.5, [255, 0, 0], [0, 0, 255]),
            (Way::Arrives, 0.5, [0, 0, 255], [255, 0, 0]),
            (Way::Through, 0.25, plate, [255, 0, 0]),
            (Way::Through, 0.75, plate, [0, 0, 255]),
        ];
        for (way, progress, above, below) in cases {
            let motion = Motion::new(Style::Curtain, way);
            let at = transition(&mut c, motion, progress);
            assert!(near(at(32, 2), above), "{motion}: {:?} above", at(32, 2));
            assert!(near(at(32, 37), below), "{motion}: {:?} below", at(32, 37));
        }
    }

    /// Halfway through a login in depth, the session landing from the distance covers the middle
    /// but not yet the corners, where only the greeter flying past shows, over the plate: at a
    /// quarter, as it fades out faster than the session fades in.
    #[test]
    fn a_depth_lands_the_session_from_a_distance() {
        let Some(mut c) = compositor() else { return };
        let motion = Motion::new(Style::Depth, Way::Leaves);
        let at = transition(&mut c, motion, 0.5);
        let [p, ..] = PLATE;
        let over = |under: u8, c: u8, a: f32| f32::from(under) * (1. - a) + f32::from(c) * a;
        let blend = |under: u8, c: u8, a: f32| over(under, c, a).round() as u8;
        // Corner: the plate, then red at a quarter.
        assert!(
            near(
                at(0, 0),
                [blend(p, 255, 0.25), blend(p, 0, 0.25), blend(p, 0, 0.25)]
            ),
            "{:?}",
            at(0, 0)
        );
        // Middle: the plate, blue at half, then red at a quarter.
        let mid = [
            over(over(p, 0, 0.5).round() as u8, 255, 0.25).round() as u8,
            over(over(p, 0, 0.5).round() as u8, 0, 0.25).round() as u8,
            over(over(p, 255, 0.5).round() as u8, 0, 0.25).round() as u8,
        ];
        assert!(near(at(32, 20), mid), "{:?} for {mid:?}", at(32, 20));

        // Over a black backdrop, as between two screens of the splash: no plate in the corner.
        let black = Motion {
            backdrop: [0; 3],
            ..motion
        };
        let at = transition(&mut c, black, 0.5);
        assert!(
            near(at(0, 0), [blend(0, 255, 0.25), 0, 0]),
            "{:?}",
            at(0, 0)
        );
    }

    /// The installed theme, drawn where the layout says: the watermark, the spinner, the
    /// progress bar's two colours and a message's ink, on black everywhere else. The image
    /// positions come from the layout for the images actually installed (CI's distribution ships
    /// its own); `splash::tests` pins the numbers for Fedora's.
    /// The pixels of `rect` nearest `want` in colour, by the largest channel difference.
    fn nearest(
        (width, bytes): &(i32, Vec<u8>),
        rect: Rectangle<i32, Physical>,
        want: [u8; 3],
    ) -> u8 {
        let mut best = u8::MAX;
        for y in rect.loc.y..rect.loc.y + rect.size.h {
            for x in rect.loc.x..rect.loc.x + rect.size.w {
                let i = ((y * width + x) * 4) as usize;
                let d = (0..3)
                    .map(|c| bytes[i + c].abs_diff(want[c]))
                    .max()
                    .unwrap();
                best = best.min(d);
            }
        }
        best
    }

    #[test]
    fn the_details_view_draws_the_console_in_its_colours() {
        let Some(mut c) = compositor() else { return };
        let mut splash = Splash {
            shown: true,
            details: true,
            messages: vec!["not in the view".into()],
            ..Splash::default()
        };
        splash
            .console
            .write(b"[  \x1b[0;32mOK\x1b[0m  ] Started a unit.\n");
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &splash,
                1.,
                (1280, 800),
                0,
                &Dialog::default(),
                None,
            )
            .unwrap();
        let Some(cell) = mono_cell(&mut TextShaper::new(&mut c.renderer, 1.), 1.) else {
            eprintln!("skipping: no Adwaita Mono");
            return;
        };
        let px = pixels(&mut c, &mut texture);
        // "OK" in the VT's green, three cells in; the brackets in white around it.
        let ok = rect(cell.w / 2 + 3 * cell.w, 0, 2 * cell.w, cell.h);
        assert!(nearest(&px, ok, [0, 0xaa, 0]) < 24);
        let bracket = rect(cell.w / 2, 0, cell.w, cell.h);
        assert!(nearest(&px, bracket, [0xff, 0xff, 0xff]) < 24);
        // Nothing past the first line but the watermark: no spinner, no message.
        assert_eq!(brightest(&px, rect(0, cell.h, 1280, 600)), 0);
    }

    #[test]
    fn the_splash_draws_the_theme() {
        if !geo::watermark_file(Path::new(splash::THEME_DIR)).exists() {
            eprintln!("skipping: no plymouth spinner theme");
            return;
        }
        let Some(mut c) = compositor() else { return };

        let boot = Splash {
            shown: true,
            messages: vec!["Hello".into()],
            ..Splash::default()
        };
        // Mid-turn: a theme's first frames can be all but blank, as Ubuntu's fade in (its first
        // peaks at 4/255).
        let frame = c.spinner_frames() / 2;
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &boot,
                1.,
                (1280, 800),
                frame,
                &Dialog::default(),
                None,
            )
            .unwrap();
        let images = c.theme.as_ref().unwrap().images((1280, 800).into(), 1.);
        let (watermark, spinner) = (images.watermark.unwrap(), images.spinner.unwrap().0);
        let items = splash::layout(
            &boot,
            (1280, 800).into(),
            &images,
            frame,
            &Dialog::default(),
            &mut |_, _| (0, 0).into(),
        );
        let place = |item: &Item| match item {
            Item::Watermark(at) => Some(rect(at.x, at.y, watermark.w, watermark.h)),
            Item::Spinner { at, .. } => Some(rect(at.x, at.y, spinner.w, spinner.h)),
            _ => None,
        };
        let [wm, sp] = [0, 1].map(|i| place(&items[i]).unwrap());
        let px = pixels(&mut c, &mut texture);
        assert!(brightest(&px, wm) > 128);
        assert!(brightest(&px, sp) > 64);
        // The message, under the spinner.
        let below = sp.loc.y + sp.size.h;
        assert!(brightest(&px, rect(540, below, 200, 40)) > 128);
        // Black above the spinner.
        assert_eq!(brightest(&px, rect(0, 0, 1280, sp.loc.y)), 0);

        let mut update = Splash {
            mode: Mode::Updates,
            shown: true,
            progress: Some(50),
            ..Splash::default()
        };
        // Each picture after the first is redrawn into the one before, as the outer does while the
        // spinner turns: nothing of the previous splash may show through.
        let reuse = Some(texture.clone());
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &update,
                1.,
                (1280, 800),
                0,
                &Dialog::default(),
                reuse,
            )
            .unwrap();
        let px = pixels(&mut c, &mut texture);
        // The bar at 440×556, 400×5: white to half, grey after.
        let (width, bytes) = &px;
        let at = |x: i32, y: i32| bytes[((y * width + x) * 4) as usize];
        assert_eq!(at(540, 558), 255);
        assert_eq!(at(740, 558), 0x60);
        // The title, near 0.382 of the height, and no spinner.
        assert!(brightest(&px, rect(400, 250, 480, 60)) > 128);
        // (The bar crosses the spinner's square from 556 down.)
        let above_bar = rect(sp.loc.x, sp.loc.y, sp.size.w, 556 - sp.loc.y);
        assert_eq!(brightest(&px, above_bar), 0);

        // A password prompt: the dialog where the layout puts it, its bullets, no spinner.
        let asking = Splash {
            shown: true,
            password_prompt: Some(super::super::plymouth::Prompt {
                text: Some("Passphrase:".into()),
            }),
            ..Splash::default()
        };
        let dialog = Dialog {
            bullets: 3,
            caps_lock: false,
            keymap: Some("English (US)".into()),
        };
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &asking,
                1.,
                (1280, 800),
                0,
                &dialog,
                Some(texture.clone()),
            )
            .unwrap();
        let laid = splash::layout(
            &asking,
            (1280, 800).into(),
            &images,
            0,
            &dialog,
            &mut |_, _| (0, 0).into(),
        );
        let px = pixels(&mut c, &mut texture);
        for item in &laid {
            let (at, size) = match item {
                Item::Lock(at) => (at, images.lock),
                Item::Entry(at) => (at, images.entry),
                Item::Keyboard(at) => (at, images.keyboard),
                Item::Bullet { at, src } => (at, Some(src.size)),
                _ => continue,
            };
            let size = size.unwrap();
            assert!(
                brightest(&px, rect(at.x, at.y, size.w, size.h)) > 64,
                "{item:?}"
            );
        }
        assert_eq!(brightest(&px, sp), 0);

        // The retained frame: the watermark alone.
        update.shown = false;
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &update,
                1.,
                (1280, 800),
                0,
                &Dialog::default(),
                Some(texture.clone()),
            )
            .unwrap();
        let px = pixels(&mut c, &mut texture);
        assert!(brightest(&px, wm) > 128);
        assert_eq!(brightest(&px, rect(0, 0, 1280, wm.loc.y)), 0);
    }

    /// A BGRT table as the kernel exports it, in a fresh directory: a `w`×`h` logo of one
    /// colour (0xRRGGBB), its top-left at `offset`.
    fn bgrt_table(
        name: &str,
        w: usize,
        h: usize,
        rgb: u32,
        offset: (i32, i32),
    ) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("synoik-bgrt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pitch = (3 * w + 3) & !3;
        let mut bmp = Vec::new();
        bmp.extend(b"BM");
        bmp.extend(((54 + pitch * h) as u32).to_le_bytes());
        bmp.extend(0u32.to_le_bytes());
        bmp.extend(54u32.to_le_bytes());
        bmp.extend(40u32.to_le_bytes());
        bmp.extend((w as i32).to_le_bytes());
        bmp.extend((h as i32).to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend([0; 24]);
        for _ in 0..h {
            for _ in 0..w {
                bmp.extend([rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8]);
            }
            bmp.extend(vec![0; pitch - 3 * w]);
        }
        std::fs::write(dir.join("image"), bmp).unwrap();
        std::fs::write(dir.join("status"), "1\n").unwrap();
        std::fs::write(dir.join("xoffset"), format!("{}\n", offset.0)).unwrap();
        std::fs::write(dir.join("yoffset"), format!("{}\n", offset.1)).unwrap();
        dir
    }

    /// The firmware's logo where the layout puts it, in its own colours, as Limina's firmware
    /// gives it at 1280×800; gone under a password prompt.
    #[test]
    fn the_splash_draws_the_firmware_logo() {
        let Some(mut c) = compositor() else { return };
        let dir = bgrt_table("compose", 256, 256, 0x20_80ff, (512, 272));
        c.theme = Some(Theme::load(
            &mut c.renderer,
            Path::new(splash::THEME_DIR),
            &dir,
        ));
        std::fs::remove_dir_all(&dir).unwrap();

        let boot = Splash {
            shown: true,
            ..Splash::default()
        };
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &boot,
                1.,
                (1280, 800),
                0,
                &Dialog::default(),
                None,
            )
            .unwrap();
        let px = pixels(&mut c, &mut texture);
        let (width, bytes) = &px;
        let at = |x: i32, y: i32| {
            let i = ((y * width + x) * 4) as usize;
            [bytes[i], bytes[i + 1], bytes[i + 2]]
        };
        assert_eq!(at(512, 272), [0x20, 0x80, 0xff]);
        assert_eq!(at(767, 527), [0x20, 0x80, 0xff]);
        assert_eq!(at(511, 272), [0, 0, 0]);
        assert_eq!(at(512, 271), [0, 0, 0]);

        let asking = Splash {
            shown: true,
            password_prompt: Some(super::super::plymouth::Prompt { text: None }),
            ..Splash::default()
        };
        let mut texture = c
            .splash_picture(
                (1280, 800),
                &asking,
                1.,
                (1280, 800),
                0,
                &Dialog::default(),
                Some(texture),
            )
            .unwrap();
        let px = pixels(&mut c, &mut texture);
        assert_eq!(brightest(&px, rect(512, 272, 256, 16)), 0);
    }
}
