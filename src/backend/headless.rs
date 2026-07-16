//! Headless backend for tests.
//!
//! This can eventually grow into a more complete backend if needed, but for now it's missing some
//! crucial parts like dmabufs.

use std::mem;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use niri_config::OutputName;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::egl::native::EGLSurfacelessDisplay;
use smithay::backend::egl::{EGLContext, EGLDisplay};
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::Size;
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult, RendererKind};
use crate::niri::{Niri, RedrawState};
use crate::render_helpers::{resources, shaders};
use crate::utils::{get_monotonic_time, logical_output};

/// The headless backend's optional renderer. Clients need one to draw (and for screencasting);
/// the compositor logic itself is driveable over IPC with none. It can be either the default
/// GLES renderer or — behind `--renderer=vulkan` + the `vulkan` feature — the owned Vulkan one.
///
/// The `Vulkan` variant keeps a GLES renderer **alongside** the Vulkan one, mirroring the Tty
/// backend (where the GLES `gpu_manager` coexists with `vulkan_renderer`). That coexistence is on
/// its way out (slice 8): the snapshot-based animations that used to bake through GLES here — the
/// resize crossfade in particular — are Vulkan-native now. What still holds it up is the set of
/// `with_primary_renderer` call sites listed in `docs/fork/renderer-gaps.md`.
///
/// Note the EGL display this drags in is also what keeps the suite from running on lavapipe (Mesa
/// redirects Zink's EGL and it fails to pick a pdev); dropping the GLES half is what frees it.
#[allow(clippy::large_enum_variant)]
enum HeadlessRenderer {
    Gles(GlesRenderer),
    #[cfg(feature = "vulkan")]
    Vulkan {
        gles: GlesRenderer,
        // Read by the test-only `with_vulkan_renderer`; the live headless `render` is a no-op.
        #[cfg_attr(not(test), allow(dead_code))]
        vulkan: crate::render_helpers::vulkan::VulkanRenderer,
    },
}

pub struct Headless {
    kind: RendererKind,
    renderer: Option<HeadlessRenderer>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
}

impl Headless {
    pub fn new(kind: RendererKind) -> Self {
        Self {
            kind,
            renderer: None,
            ipc_outputs: Default::default(),
        }
    }

    pub fn using_vulkan(&self) -> bool {
        self.kind == RendererKind::Vulkan
    }

    pub fn init(&mut self, _niri: &mut Niri) {}

    pub fn add_renderer(&mut self) -> anyhow::Result<()> {
        if self.renderer.is_some() {
            error!("add_renderer: renderer must not already exist");
            return Ok(());
        }

        self.renderer = Some(match self.kind {
            RendererKind::Gles => {
                let mut renderer = unsafe {
                    let display = EGLDisplay::new(EGLSurfacelessDisplay)
                        .context("error creating EGL display")?;
                    let context =
                        EGLContext::new(&display).context("error creating EGL context")?;
                    GlesRenderer::new(context).context("error creating renderer")?
                };
                resources::init(&mut renderer);
                shaders::init(&mut renderer);
                HeadlessRenderer::Gles(renderer)
            }
            #[cfg(feature = "vulkan")]
            RendererKind::Vulkan => {
                // The owned Vulkan renderer brings up its own instance/device (no EGL, no
                // surface) and manages its own pipelines — no resources/shaders init needed.
                let vulkan = crate::render_helpers::vulkan::VulkanRenderer::new()
                    .context("error creating the Vulkan renderer")?;
                // Also build a GLES renderer, as the Tty backend does, for snapshot capture (the
                // resize crossfade's neutral buffer is rendered through it even on Vulkan).
                let mut gles = unsafe {
                    let display = EGLDisplay::new(EGLSurfacelessDisplay)
                        .context("error creating EGL display")?;
                    let context =
                        EGLContext::new(&display).context("error creating EGL context")?;
                    GlesRenderer::new(context).context("error creating GLES renderer")?
                };
                resources::init(&mut gles);
                shaders::init(&mut gles);
                HeadlessRenderer::Vulkan { gles, vulkan }
            }
            #[cfg(not(feature = "vulkan"))]
            RendererKind::Vulkan => {
                // Unreachable: State::new rejects --renderer=vulkan without the feature.
                anyhow::bail!("niri was built without the `vulkan` feature");
            }
        });
        Ok(())
    }

    pub fn add_output(&mut self, niri: &mut Niri, n: u8, size: (u16, u16)) {
        let connector = format!("headless-{n}");
        let make = "niri".to_string();
        let model = "headless".to_string();
        let serial = n.to_string();

        let output = Output::new(
            connector.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: make.clone(),
                model: model.clone(),
                serial_number: serial.clone(),
            },
        );

        let mode = Mode {
            size: Size::from((i32::from(size.0), i32::from(size.1))),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, None);
        output.set_preferred(mode);

        output.user_data().insert_if_missing(|| OutputName {
            connector,
            make: Some(make),
            model: Some(model),
            serial: Some(serial),
        });

        let physical_properties = output.physical_properties();
        self.ipc_outputs.lock().unwrap().insert(
            OutputId::next(),
            niri_ipc::Output {
                name: output.name(),
                make: physical_properties.make,
                model: physical_properties.model,
                serial: None,
                physical_size: None,
                modes: vec![niri_ipc::Mode {
                    width: size.0,
                    height: size.1,
                    refresh_rate: 60_000,
                    is_preferred: true,
                }],
                current_mode: Some(0),
                is_custom_mode: true,
                vrr_supported: false,
                vrr_enabled: false,
                logical: Some(logical_output(&output)),
                max_bpc: None,
            },
        );

        niri.add_output(output, None, false);
    }

    pub fn seat_name(&self) -> String {
        "headless".to_owned()
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut GlesRenderer) -> T,
    ) -> Option<T> {
        // This accessor is GLES-typed. On a Vulkan session it hands out the coexisting GLES
        // renderer (as the Tty backend does), so GLES-only tasks — screencast setup, snapshot
        // capture — keep working.
        match &mut self.renderer {
            Some(HeadlessRenderer::Gles(renderer)) => Some(f(renderer)),
            #[cfg(feature = "vulkan")]
            Some(HeadlessRenderer::Vulkan { gles, .. }) => Some(f(gles)),
            None => None,
        }
    }

    /// Access to the owned Vulkan renderer, so the capture paths (screencopy, screenshot) and the
    /// headless tests can drive the real `Niri::render` through it. Returns `None` for a GLES
    /// backend.
    #[cfg(feature = "vulkan")]
    pub fn with_vulkan_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut crate::render_helpers::vulkan::VulkanRenderer) -> T,
    ) -> Option<T> {
        match &mut self.renderer {
            Some(HeadlessRenderer::Vulkan { vulkan, .. }) => Some(f(vulkan)),
            _ => None,
        }
    }

    pub fn render(&mut self, niri: &mut Niri, output: &Output) -> RenderResult {
        let states = RenderElementStates::default();
        let mut presentation_feedbacks = niri.take_presentation_feedbacks(output, &states);
        presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
            get_monotonic_time(),
            Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );

        let output_state = niri.output_state.get_mut(output).unwrap();
        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => unreachable!(),
        }

        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        // FIXME: request redraw on unfinished animations remain

        RenderResult::Submitted
    }

    pub fn import_dmabuf(&mut self, _dmabuf: &Dmabuf) -> bool {
        unimplemented!()
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }
}

impl Default for Headless {
    fn default() -> Self {
        Self::new(RendererKind::default())
    }
}

#[cfg(all(test, feature = "vulkan"))]
mod tests {
    use super::*;
    use crate::render_helpers::vulkan::VulkanRenderer;

    // `--renderer=vulkan` end-to-end on the headless backend: the stored kind drives
    // `add_renderer` to build the owned Vulkan renderer plus a coexisting GLES renderer (mirroring
    // Tty), so the GLES-typed primary-renderer accessor still hands one out. Skips with no device,
    // matching the other Vulkan tests.
    #[test]
    fn headless_builds_the_vulkan_renderer() {
        if let Err(e) = VulkanRenderer::new() {
            eprintln!("skipping headless_builds_the_vulkan_renderer: no Vulkan device ({e})");
            return;
        }

        let mut backend = Headless::new(RendererKind::Vulkan);
        backend
            .add_renderer()
            .expect("headless should build the Vulkan renderer");
        assert!(matches!(
            backend.renderer,
            Some(HeadlessRenderer::Vulkan { .. })
        ));
        // The coexisting GLES renderer is available for snapshot capture.
        assert!(backend.with_primary_renderer(|_| ()).is_some());
    }
}
