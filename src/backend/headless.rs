//! Headless backend for tests.
//!
//! This can eventually grow into a more complete backend if needed, but for now it's missing some
//! crucial parts like dmabufs.

use std::mem;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use niri_config::OutputName;
use smithay::backend::allocator::dmabuf::Dmabuf;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::allocator::gbm::GbmDevice;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::reexports::rustix::fs::{self as rfs, OFlags};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::utils::DeviceFd;
use smithay::utils::Size;
use smithay::wayland::presentation::Refresh;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::niri::{Niri, RedrawState};
use crate::utils::{get_monotonic_time, logical_output};

/// The headless backend's optional renderer. Clients need one to draw (and for screencasting);
/// the compositor logic itself is driveable over IPC with none.
pub struct Headless {
    // Read by the test-only `with_vulkan_renderer`; the live headless `render` is a no-op.
    #[cfg_attr(not(test), allow(dead_code))]
    renderer: Option<crate::render_helpers::vulkan::VulkanRenderer>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    /// The VT a [`change_vt`](Self::change_vt) last asked for.
    ///
    /// Headless has no VTs, so the switch itself is a no-op — but *whether the request got here*
    /// is exactly what a test of the lock screen's escape hatch needs to know, and the alternative
    /// is asserting on some proxy that can agree while the real path is broken.
    last_vt: Option<i32>,
    /// A GBM device on the render node, opened lazily the first time a screencast asks for one.
    ///
    /// Screencasting refuses to start without one ([`State::prepare_pw_cast`]), which used to make
    /// the whole cast path untestable headless — and that path is exactly the one that has needed
    /// a fast, seat-free reproduction. A *render* node is enough for GBM allocation: no DRM
    /// master, no session, no VT.
    ///
    /// [`State::prepare_pw_cast`]: crate::niri::State
    #[cfg(feature = "xdp-gnome-screencast")]
    gbm: Option<GbmDevice<DrmDeviceFd>>,
}

impl Headless {
    pub fn new() -> Self {
        Self {
            renderer: None,
            ipc_outputs: Default::default(),
            last_vt: None,
            #[cfg(feature = "xdp-gnome-screencast")]
            gbm: None,
        }
    }

    /// A GBM device for screencast buffer allocation, opened on first use.
    ///
    /// Returns `None` when there is no usable render node, which is a normal state for a headless
    /// run on a machine with no GPU — the caller reports it as "no GBM device available" and the
    /// cast simply does not start.
    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn gbm_device(&mut self) -> Option<GbmDevice<DrmDeviceFd>> {
        if let Some(gbm) = &self.gbm {
            return Some(gbm.clone());
        }

        let path = std::env::var("NIRI_HEADLESS_RENDER_NODE")
            .unwrap_or_else(|_| "/dev/dri/renderD128".to_owned());

        let open = |path: &str| -> anyhow::Result<GbmDevice<DrmDeviceFd>> {
            let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
            let fd = rfs::open(path, flags, rfs::Mode::empty())
                .with_context(|| format!("error opening {path}"))?;
            GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(fd)))
                .with_context(|| format!("error creating a GBM device on {path}"))
        };

        match open(&path) {
            Ok(gbm) => {
                debug!("headless screencast GBM device: {path}");
                self.gbm = Some(gbm.clone());
                Some(gbm)
            }
            Err(err) => {
                warn!("no GBM device for headless screencasting: {err:?}");
                None
            }
        }
    }

    pub fn init(&mut self, _niri: &mut Niri) {}

    /// Record the request; there is no VT to switch to.
    pub fn change_vt(&mut self, vt: i32) {
        self.last_vt = Some(vt);
    }

    /// The VT last asked for, for tests of paths that must not swallow the switch.
    pub fn last_vt(&self) -> Option<i32> {
        self.last_vt
    }

    pub fn add_renderer(&mut self) -> anyhow::Result<()> {
        if self.renderer.is_some() {
            error!("add_renderer: renderer must not already exist");
            return Ok(());
        }

        // The owned Vulkan renderer brings up its own instance/device (no EGL, no surface) and
        // manages its own pipelines — no resources/shaders init needed.
        let vulkan = crate::render_helpers::vulkan::VulkanRenderer::new()
            .context("error creating the Vulkan renderer")?;

        self.renderer = Some(vulkan);
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

    /// Access to the owned Vulkan renderer, so the capture paths (screencopy, screenshot) and the
    /// headless tests can drive the real `Niri::render` through it. Returns `None` before
    /// `add_renderer`.
    pub fn with_vulkan_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut crate::render_helpers::vulkan::VulkanRenderer) -> T,
    ) -> Option<T> {
        self.renderer.as_mut().map(f)
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
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_helpers::vulkan::VulkanRenderer;

    // `add_renderer` builds the owned Vulkan renderer. Skips with no device, matching the other
    // Vulkan tests.
    #[test]
    fn headless_builds_the_vulkan_renderer() {
        if let Err(e) = VulkanRenderer::new() {
            eprintln!("skipping headless_builds_the_vulkan_renderer: no Vulkan device ({e})");
            return;
        }

        let mut backend = Headless::new();
        backend
            .add_renderer()
            .expect("headless should build the Vulkan renderer");
        assert!(backend.with_vulkan_renderer(|_| ()).is_some());
    }
}
