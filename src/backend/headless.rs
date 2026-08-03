//! Headless backend for tests.
//!
//! This can eventually grow into a more complete backend if needed, but for now it's missing some
//! crucial parts like dmabufs.

use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use niri_config::OutputName;
use smithay::backend::allocator::dmabuf::Dmabuf;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::allocator::gbm::GbmDevice;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
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
    // Reached through `with_vulkan_renderer`: `render` itself composites nothing (there is no
    // scanout to composite for), but everything that captures the output as a side effect of the
    // redraw — screencast, screencopy, screenshots — draws through this renderer.
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

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
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
            RedrawState::Queued => (),
            // Damage landed while the next animation frame was pending (see `queue_next_frame`),
            // and that redraw is this one — so the timer has nothing left to ask for.
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => niri.event_loop.remove(token),
            RedrawState::Idle
            | RedrawState::WaitingForVBlank { .. }
            | RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
        }

        let output_state = niri.output_state.get_mut(output).unwrap();

        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        queue_next_frame(niri, output, target_presentation_time);

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

/// Ask for the next frame of an ongoing animation.
///
/// Headless has no VBlank, so nothing else ever will: a redraw leaves the output `Idle` and only
/// new damage brings it back. An animation would therefore render exactly **one** frame and then
/// sit at whatever progress it reached — and since a screencast is rendered as a side effect of
/// the redraw ([`Niri::render_captures_with`]), a cast of a headless session sees that same single
/// frame. This is the estimated-vblank timer the TTY backend keeps for the same reason (no
/// presentation time from DRM), minus the DRM parts.
///
/// [`Niri::render_captures_with`]: crate::niri::Niri
fn queue_next_frame(niri: &mut Niri, output: &Output, target_presentation_time: Duration) {
    let output_state = niri.output_state.get_mut(output).unwrap();
    if !output_state.unfinished_animations_remain {
        return;
    }

    // A zero-length timer would just spin: `render` already sent this frame's callbacks, so wait
    // out the frame interval before asking for the next one.
    let mut duration = target_presentation_time.saturating_sub(get_monotonic_time());
    if duration.is_zero() {
        duration += output_state
            .frame_clock
            .refresh_interval()
            .unwrap_or(Duration::from_micros(16_667));
    }

    let timer_output = output.clone();
    let token = niri
        .event_loop
        .insert_source(Timer::from_duration(duration), move |_, _, data| {
            on_frame_timer(&mut data.niri, &timer_output);
            TimeoutAction::Drop
        })
        .unwrap();

    // Claim the output for the duration, so a `queue_redraw` in between lands as
    // `WaitingForEstimatedVBlankAndQueued` instead of starting a second, unpaced redraw loop.
    let output_state = niri.output_state.get_mut(output).unwrap();
    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
}

/// The timer from [`queue_next_frame`] fired: hand the output back and ask for the next frame.
fn on_frame_timer(niri: &mut Niri, output: &Output) {
    let Some(output_state) = niri.output_state.get_mut(output) else {
        // The output went away while the timer was pending.
        return;
    };

    match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
        RedrawState::WaitingForEstimatedVBlank(_) => (),
        // Something damaged the output while we were waiting; that redraw is already queued.
        RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
            output_state.redraw_state = RedrawState::Queued;
            return;
        }
        RedrawState::Idle | RedrawState::Queued | RedrawState::WaitingForVBlank { .. } => {
            unreachable!()
        }
    }

    if output_state.unfinished_animations_remain {
        niri.queue_redraw(output);
    } else {
        niri.send_frame_callbacks(output);
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
