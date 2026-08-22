// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

//! Headless backend: no display and no input devices, one virtual output, driven over IPC.
//!
//! Not a test-only fork of the compositor — it runs the same `Synoik` the tty backend does, which
//! is what makes the conformance corpus worth anything. What it does not have is a scanout: there
//! is no screen to composite for, so `render` assembles the frame's elements but hands them to
//! nothing, and the capture paths (screencast, screencopy, screenshot) are what actually draw.
//!
//! It assembles them anyway because the element pass is what decides *which surfaces this output is
//! presenting* — see [`Headless::render_element_states`]. Skipping it does not save a redraw, it
//! silently mis-answers that question, and frame callbacks are the thing that notices.
//!
//! Clients present through the GPU here, same as on a real seat: [`Headless::add_dmabuf_global`]
//! advertises dmabuf off a plain render node — no DRM master, no seat, no VT.

use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use smithay::backend::allocator::dmabuf::Dmabuf;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::allocator::gbm::GbmDevice;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::ImportDma as _;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::rustix::fs as rfs;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
#[cfg(feature = "xdp-gnome-screencast")]
use smithay::utils::DeviceFd;
use smithay::utils::Size;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;
use synoik_config::OutputName;

use super::{IpcOutputMap, OutputId, RenderResult};
use crate::synoik::{RedrawState, Synoik};
use crate::utils::{get_monotonic_time, logical_output};

/// The headless backend's optional renderer. Clients need one to draw (and for screencasting);
/// the compositor logic itself is driveable over IPC with none.
pub struct Headless {
    // `render` composites nothing through it (there is no scanout to composite for), but it does
    // run the element pass on it to learn what the output is presenting
    // (`render_element_states`), and everything that captures the output as a side effect of the
    // redraw — screencast, screencopy, screenshots — draws through it. Also reached directly via
    // `with_vulkan_renderer`.
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
    /// [`State::prepare_pw_cast`]: crate::synoik::State
    #[cfg(feature = "xdp-gnome-screencast")]
    gbm: Option<GbmDevice<DrmDeviceFd>>,
    /// The `zwp_linux_dmabuf_v1` global, once [`add_dmabuf_global`](Self::add_dmabuf_global) has
    /// found a render node to advertise. `None` means clients only ever see shm.
    dmabuf_global: Option<DmabufGlobal>,
    /// Where a test may composite the frames this backend renders, and only those.
    ///
    /// Headless paints nothing persistent: it runs the element pass to learn what the output
    /// presents and throws the pixels away. A test that wants to know what a *screen* would be
    /// holding has to accumulate them itself — and the one thing it must not do is composite on
    /// its own schedule, because then it draws frames the compositor never asked for. That is not
    /// a hypothetical: a probe that composited every 16 ms tick could not see an artifact whose
    /// whole nature is a frame that was never rendered. This hands over the real element list, at
    /// the real clock, on exactly the turns the redraw machinery decided to render.
    #[cfg(test)]
    pub(crate) frame_sink: Option<FrameSink>,
    /// One damage tracker per output, kept across frames.
    ///
    /// Headless discards the damage it computes — it runs the element pass for the
    /// [`RenderElementStates`] alone. It keeps the tracker anyway because a tracker rebuilt every
    /// frame has no previous frame to compare against, so every element reads as new and every
    /// headless frame reports a full-output repaint. That makes damage the one compositor behavior
    /// the corpus cannot pin, on the backend that exists to pin behavior.
    ///
    /// Staleness is not a reason to rebuild it: [`OutputDamageTracker::from_output`] holds a weak
    /// output and reads the mode through it, and smithay damages the whole output by itself when
    /// the size or transform changes.
    damage_trackers: HashMap<Output, OutputDamageTracker>,
}

/// A test's hook into [`Headless::render`]: the renderer, the output, and the element list of a
/// frame the compositor actually decided to draw.
#[cfg(test)]
pub(crate) type FrameSink = Box<
    dyn FnMut(
        &mut crate::render_helpers::vulkan::VulkanRenderer,
        &Output,
        &[crate::synoik::OutputRenderElements],
    ),
>;

impl Headless {
    pub fn new() -> Self {
        Self {
            renderer: None,
            ipc_outputs: Default::default(),
            last_vt: None,
            #[cfg(feature = "xdp-gnome-screencast")]
            gbm: None,
            dmabuf_global: None,
            #[cfg(test)]
            frame_sink: None,
            damage_trackers: HashMap::new(),
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

        let path = std::env::var("SYNOIK_HEADLESS_RENDER_NODE")
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

    pub fn init(&mut self, _niri: &mut Synoik) {}

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

    /// Advertise `zwp_linux_dmabuf_v1` for the device the renderer actually draws on, so headless
    /// clients can present through the GPU instead of falling back to shm.
    ///
    /// Without this a headless session is blind to exactly the thing a screenshot test most wants
    /// to assert: a GPU-rendering client composites empty, so window *contents* can never be judged
    /// from a headless shot (`docs/fork/test-harness-realism.md` §1). Nothing here needs DRM
    /// master, a seat or a VT — the owned Vulkan renderer imports client dmabufs off a plain
    /// render node.
    ///
    /// The node advertised is the one the *renderer* reports (`VK_EXT_physical_device_drm`), not
    /// whatever `SYNOIK_HEADLESS_RENDER_NODE` points GBM at: feedback names the device a client
    /// should allocate for, and the only device we can import on is the one we render on. Drivers
    /// that report no node (lavapipe) get **no global at all** rather than a wrong one — a
    /// compositor advertising dmabuf it cannot import hands clients a blank window and per-frame
    /// error spam, where shm just works.
    ///
    /// Must not be split from [`import_dmabuf`](Self::import_dmabuf): creating the global is what
    /// makes that path reachable.
    pub fn add_dmabuf_global(&mut self, synoik: &mut Synoik) {
        if self.dmabuf_global.is_some() {
            error!("add_dmabuf_global: the global must not already exist");
            return;
        }

        let Some(renderer) = &self.renderer else {
            debug!("no dmabuf global: headless is running without a renderer");
            return;
        };

        let Some((major, minor)) = renderer.gpu().drm_render_node() else {
            // Normal on a software ICD; the corpus runs there.
            debug!("no dmabuf global: the Vulkan device reports no DRM render node");
            return;
        };
        let dev_id = rfs::makedev(major, minor);

        // The same format set the tty backend advertises (LINEAR 8888), from the same source of
        // truth — a client that honors this feedback allocates something `import_dmabuf` accepts.
        let formats = crate::render_helpers::vulkan::dmabuf_formats();
        let feedback = match DmabufFeedbackBuilder::new(dev_id, formats).build() {
            Ok(feedback) => feedback,
            Err(err) => {
                warn!("error building headless dmabuf feedback: {err:?}");
                return;
            }
        };

        let global = synoik
            .dmabuf_state
            .create_global_with_default_feedback::<crate::synoik::State>(
                &synoik.display_handle,
                &feedback,
            );
        self.dmabuf_global = Some(global);
        debug!("headless dmabuf global on DRM render node {major}:{minor}");
    }

    pub fn add_output(&mut self, synoik: &mut Synoik, n: u8, size: (u16, u16)) {
        let connector = format!("headless-{n}");
        let make = "synoik".to_string();
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
            vendor: None,
            model: Some(model),
            serial: Some(serial),
        });

        let physical_properties = output.physical_properties();
        self.ipc_outputs.lock().unwrap().insert(
            OutputId::next(),
            synoik_ipc::Output {
                name: output.name(),
                make: physical_properties.make,
                vendor: None,
                model: physical_properties.model,
                serial: None,
                physical_size: None,
                modes: vec![synoik_ipc::Mode {
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

        synoik.add_output(output, None, false);
    }

    pub fn seat_name(&self) -> String {
        "headless".to_owned()
    }

    /// Access to the owned Vulkan renderer, so the capture paths (screencopy, screenshot) and the
    /// headless tests can drive the real `Synoik::render` through it. Returns `None` before
    /// `add_renderer`.
    pub fn with_vulkan_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut crate::render_helpers::vulkan::VulkanRenderer) -> T,
    ) -> Option<T> {
        self.renderer.as_mut().map(f)
    }

    /// Which surfaces this redraw is presenting, from the same element pass the tty backend runs.
    ///
    /// Headless has no scanout, but it still owes an answer to "is this surface being presented on
    /// this output": [`Synoik::send_frame_callbacks`] filters on it, and a surface that answers
    /// "no" is left to smithay's *overdue* path — one callback per `FRAME_CALLBACK_THROTTLE`
    /// (995 ms), which paces a self-pacing client at 1 fps. This handed back an empty
    /// [`RenderElementStates`] until 2026-08-10, so that was every headless client; pinned now by
    /// `a_client_is_paced_by_the_redraw_not_the_overdue_fallback`.
    ///
    /// The answer comes from the real element pass on purpose. A cheaper "every mapped window is on
    /// its output" shortcut would be a second notion of visibility, free to drift from the one the
    /// tty backend and the capture paths use — and this is exactly the question the corpus exists
    /// to answer the same way the real compositor does.
    ///
    /// `damage_output` computes coverage and occlusion without drawing; the damage it returns is
    /// discarded here, but the tracker is kept per output so the damage it computes is real — see
    /// [`Headless::damage_trackers`].
    fn render_element_states(&mut self, synoik: &Synoik, output: &Output) -> RenderElementStates {
        let Some(renderer) = &mut self.renderer else {
            // No renderer means no dmabuf global and inert capture paths, so nothing is drawing
            // that a frame callback could pace. Overdue is then the honest answer, not a fallback.
            return RenderElementStates::default();
        };

        let ctx = crate::render_helpers::RenderCtx {
            renderer,
            target: crate::render_helpers::RenderTarget::Output,
            appearance: Some(synoik.appearance()),
        };
        let elements = synoik.render_to_vec(ctx, output, true);

        // Attribute the frame's damage inputs *before* any debug overlay is spliced in: the overlay
        // z-shifts everything below it, and an instrument that reads its own presence is worse than
        // none.
        crate::frame_log::log_damage_attribution(
            &output.name(),
            &elements,
            output.current_scale().fractional_scale().into(),
            smithay::utils::Rectangle::from_size(
                output
                    .current_mode()
                    .map_or_else(Default::default, |m| m.size),
            ),
        );

        #[cfg(test)]
        if let Some(sink) = &mut self.frame_sink {
            let renderer = self.renderer.as_mut().expect("checked just above");
            sink(renderer, output, &elements);
        }

        let damage_tracker = self
            .damage_trackers
            .entry(output.clone())
            .or_insert_with(|| OutputDamageTracker::from_output(output));
        match damage_tracker.damage_output(1, &elements) {
            Ok((_damage, states)) => states,
            Err(err) => {
                warn!("error computing headless render element states: {err:?}");
                RenderElementStates::default()
            }
        }
    }

    pub fn render(
        &mut self,
        synoik: &mut Synoik,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        let states = self.render_element_states(synoik, output);
        synoik.update_primary_scanout_output(output, &states);

        let mut presentation_feedbacks = synoik.take_presentation_feedbacks(output, &states);
        presentation_feedbacks.presented::<_, smithay::utils::Monotonic>(
            get_monotonic_time(),
            Refresh::Unknown,
            0,
            wp_presentation_feedback::Kind::empty(),
        );

        let output_state = synoik.output_state.get_mut(output).unwrap();
        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Queued => (),
            // Damage landed while the next animation frame was pending (see `queue_next_frame`),
            // and that redraw is this one — so the timer has nothing left to ask for.
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                synoik.event_loop.remove(token)
            }
            RedrawState::Idle
            | RedrawState::ScheduledDispatch { .. }
            | RedrawState::WaitingForVBlank { .. }
            | RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
        }

        let output_state = synoik.output_state.get_mut(output).unwrap();

        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        // Headless presents inside the render call — there is no flip to come back from — so this
        // is the same moment the TTY reaches on a vblank.
        if mem::take(&mut output_state.shield_frame_queued) {
            synoik.note_shield_frame_presented(output);
        }

        queue_next_frame(synoik, output, target_presentation_time);

        RenderResult::Submitted
    }

    /// The `DmabufHandler` validation callback: the one site that decides which client buffers are
    /// accepted. It has to answer from the *renderer*, not from the feedback we advertised — a
    /// client is free to ignore feedback, and a tiled or multi-planar buffer waved through here
    /// fails later at render time, which reads as a blank window rather than a rejected buffer.
    /// Same reasoning, and same import call, as the tty path.
    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        // Unreachable without a global, which is only created when there is a renderer — but the
        // answer for "we cannot import this" is false, never a panic.
        let Some(renderer) = &mut self.renderer else {
            return false;
        };

        match renderer.import_dmabuf(dmabuf, None) {
            Ok(_texture) => true,
            Err(err) => {
                debug!("error importing dmabuf into the Vulkan renderer: {err:?}");
                false
            }
        }
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
/// the redraw ([`Synoik::render_captures_with`]), a cast of a headless session sees that same
/// single frame. This is the estimated-vblank timer the TTY backend keeps for the same reason (no
/// presentation time from DRM), minus the DRM parts.
///
/// [`Synoik::render_captures_with`]: crate::synoik::Synoik
fn queue_next_frame(synoik: &mut Synoik, output: &Output, target_presentation_time: Duration) {
    let output_state = synoik.output_state.get_mut(output).unwrap();
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
    let token = synoik
        .event_loop
        .insert_source(Timer::from_duration(duration), move |_, _, data| {
            on_frame_timer(&mut data.synoik, &timer_output);
            TimeoutAction::Drop
        })
        .unwrap();

    // Claim the output for the duration, so a `queue_redraw` in between lands as
    // `WaitingForEstimatedVBlankAndQueued` instead of starting a second, unpaced redraw loop.
    let output_state = synoik.output_state.get_mut(output).unwrap();
    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
}

/// The timer from [`queue_next_frame`] fired: hand the output back and ask for the next frame.
fn on_frame_timer(synoik: &mut Synoik, output: &Output) {
    let Some(output_state) = synoik.output_state.get_mut(output) else {
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
        RedrawState::Idle
        | RedrawState::Queued
        | RedrawState::ScheduledDispatch { .. }
        | RedrawState::WaitingForVBlank { .. } => {
            unreachable!()
        }
    }

    if output_state.unfinished_animations_remain {
        synoik.queue_redraw(output);
    } else {
        synoik.send_frame_callbacks(output);
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
