// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The outer's own drawing: what it composites during a session switch (§5) and nothing else.
//!
//! Two pictures, crossfaded into one of the outer's own scanout buffers. A picture is either a
//! flat colour (the plate) or a texture: a snapshot the outer took of what was on screen, or a
//! session frame imported from its dmabuf. Steady state never comes here — a fronted session's
//! buffers are flipped as they are.

use std::os::fd::OwnedFd;
use std::rc::Rc;

use anyhow::{anyhow, Context as _};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::{Allocator as _, Fourcc, Modifier};
use smithay::backend::drm::{DrmDeviceFd, DrmNode};
use smithay::backend::renderer::{
    Bind as _, Color32F, Frame as _, ImportDma as _, Offscreen as _, Renderer as _, Texture as _,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

use crate::backend::vulkan_scanout::{
    framebuffer_from_dmabuf, PrimeFramebuffer, VulkanScanoutAllocator, VulkanScanoutBuffer,
};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer, NATIVE_FOURCC};

/// The outer's scanout buffers: one on screen, one to draw the next frame into. Flips go one at a
/// time, so a third would never be free when the other two are not.
const SLOTS: usize = 2;

#[derive(Clone)]
pub(super) enum Picture {
    Color([f32; 3]),
    Texture(VkTexture),
}

pub(super) struct Compositor {
    renderer: VulkanRenderer,
    node: Option<DrmNode>,
    size: Size<i32, Physical>,
    slots: Vec<Slot>,
}

struct Slot {
    _buffer: VulkanScanoutBuffer,
    dmabuf: Dmabuf,
    fb: Rc<PrimeFramebuffer>,
}

impl Compositor {
    pub fn new(render_node: (u64, u64), size: (u16, u16)) -> anyhow::Result<Self> {
        let (major, minor) = render_node;
        let renderer = VulkanRenderer::for_drm_render_node(major as u32, minor as u32)
            .map_err(|err| anyhow!("bringing up Vulkan on {major}:{minor}: {err:?}"))?;
        let node = DrmNode::from_dev_id(libc::makedev(major as u32, minor as u32)).ok();
        Ok(Self {
            renderer,
            node,
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            slots: Vec::new(),
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
        self.capture(&source, &source, 0.)
    }

    /// `from` crossfaded towards `to` at `alpha`, drawn into a texture the outer owns.
    pub fn capture(
        &mut self,
        from: &Picture,
        to: &Picture,
        alpha: f32,
    ) -> anyhow::Result<VkTexture> {
        let size = self.size;
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

    /// Draw `from` crossfaded towards `to` at `t` (0 is all `from`) into a free scanout slot,
    /// avoiding `busy`. Returns the slot, its framebuffer and the fence that completes the draw.
    pub fn compose(
        &mut self,
        drm_fd: &DrmDeviceFd,
        busy: Option<usize>,
        from: &Picture,
        to: &Picture,
        t: f32,
    ) -> anyhow::Result<(usize, Rc<PrimeFramebuffer>, Option<OwnedFd>)> {
        if self.slots.is_empty() {
            self.allocate(drm_fd)?;
        }
        let index = (0..self.slots.len())
            .find(|&i| Some(i) != busy)
            .context("no free scanout slot")?;
        let size = self.size;
        let full = Rectangle::from_size(size);

        // The fence goes to the kernel as `IN_FENCE_FD`, so the submit may stay in flight.
        self.renderer.set_finish_may_defer(true);
        let result = (|| {
            let slot = &mut self.slots[index];
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
        Ok((index, self.slots[index].fb.clone(), fence))
    }

    fn allocate(&mut self, drm_fd: &DrmDeviceFd) -> anyhow::Result<()> {
        let mut allocator = VulkanScanoutAllocator::new(self.renderer.gpu().clone(), self.node)?;
        for _ in 0..SLOTS {
            // The format and layout a session's own scanout buffers use (`backend::nested`).
            let buffer = allocator
                .create_buffer(
                    self.size.w as u32,
                    self.size.h as u32,
                    Fourcc::Xrgb8888,
                    &[Modifier::Linear],
                )
                .map_err(|err| anyhow!("{err}"))?;
            let dmabuf = buffer.export().context("exporting a scanout slot")?;
            let fb = framebuffer_from_dmabuf(drm_fd, &dmabuf, true)
                .map_err(|err| anyhow!("adding a scanout slot framebuffer: {err}"))?;
            self.slots.push(Slot {
                _buffer: buffer,
                dmabuf,
                fb: Rc::new(fb),
            });
        }
        Ok(())
    }
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
