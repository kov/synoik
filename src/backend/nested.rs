// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The nested backend: a session compositor running inside the system compositor
//! (`docs/fork/system-compositor.md`).
//!
//! It is the tty backend with the display taken away. Input is the same — real evdev fds taken
//! from logind, real libinput (§2) — and so is rendering, into scanout buffers allocated on our
//! own Vulkan device. What changes is where a finished frame goes: instead of an atomic commit it
//! is handed to the outer as a buffer id plus its render fence, and the flip comes back as a
//! `Presented` carrying the kernel's own timestamp, which is what the frame clock, presentation
//! feedback and frame callbacks run on exactly as they would off a vblank (§3.1).
//!
//! The outer may go away and come back (§7). The session does not die with it: frames are dropped
//! while it is gone, the connection is retried, and on reattach every buffer is registered again
//! and the next frame is drawn in full.

use std::collections::HashMap;
use std::mem;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::{Allocator as _, Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::DrmNode;
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::{Bind as _, ImportDma as _};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session as _};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    Interest, LoopHandle, Mode as CalloopMode, PostAction, RegistrationToken,
};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs as rfs;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::utils::Size;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;

use super::vulkan_scanout::{VulkanScanoutAllocator, VulkanScanoutBuffer};
use super::{IpcOutputMap, OutputId, RenderResult};
use crate::backend::tty::{
    applied_config_is_stale, choose_target_mode, forget_applied_config, output_name_from_edid,
    AdvertisedMode,
};
use crate::frame_clock::FrameClock;
use crate::monitors_xml::MonitorsConfig;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::synoik::{RedrawState, State, Synoik};
use crate::system_compositor::protocol::{
    self, ConnectorInfo, ModeInfo, OutputConfig, Received, ToInner, ToOuter,
};
use crate::utils::{get_monotonic_time, is_laptop_panel};

/// Scanout buffers per output: one on screen, one in flight, one to render the next frame into.
const SLOTS: usize = 3;

/// How long to wait between attempts to reach an outer that went away.
const RECONNECT_INTERVAL: Duration = Duration::from_millis(500);

pub struct Nested {
    session: LibSeatSession,
    libinput: Libinput,
    handle: LoopHandle<'static, State>,
    socket_path: PathBuf,
    conn: Option<Connection>,
    renderer: VulkanRenderer,
    render_node: Option<DrmNode>,
    dmabuf_global: Option<DmabufGlobal>,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    /// Every display the outer has connected, as it last described them.
    connectors: Vec<ConnectorInfo>,
    /// Each connector's id in `ipc_outputs`, stable while it stays connected.
    output_ids: HashMap<String, OutputId>,
    /// The displays this session lights, by connector name.
    outputs: HashMap<String, NestedOutput>,
    next_buffer_id: u32,
}

/// A live connection to the outer.
struct Connection {
    fd: Rc<OwnedFd>,
    token: RegistrationToken,
}

struct NestedOutput {
    output: Output,
    /// The display, as the outer described it when this output was made for it.
    connector: ConnectorInfo,
    /// The mode this session runs it at; its buffers are this size.
    mode: ModeInfo,
    damage_tracker: OutputDamageTracker,
    slots: Vec<Slot>,
    /// Frames rendered, from 1, so that a slot's `0` can mean "never drawn".
    frame: u64,
    /// The slot whose `Present` the outer has not answered yet, with what that frame owes when it
    /// lands.
    in_flight: Option<(usize, OutputPresentationFeedback, Duration)>,
    /// The slot the outer last reported on screen. Never rendered into: it is being scanned out.
    on_screen: Option<usize>,
    /// The buffer that was in flight when the mode changed: the outer still answers it, and that
    /// answer is expected, not a protocol slip.
    abandoned: Option<u32>,
}

struct Slot {
    id: u32,
    /// Holds the Vulkan image; the dmabuf below is its export.
    _buffer: VulkanScanoutBuffer,
    dmabuf: Dmabuf,
    /// The frame this slot was last drawn into, `0` for never — or for "contents unknown", which
    /// is what a reattach makes of every slot.
    drawn: u64,
}

impl Nested {
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        socket_path: PathBuf,
    ) -> anyhow::Result<Self> {
        let (session, notifier) =
            LibSeatSession::new().context("creating a logind session for input")?;
        let seat_name = session.seat();

        // Input exactly as the tty backend takes it: logind hands out evdev fds and revokes them
        // when this session is not the active one (§2.2). The card is never asked for — the outer
        // holds it.
        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        libinput
            .udev_assign_seat(&seat_name)
            .map_err(|()| anyhow!("error assigning the seat to libinput"))?;
        if !session.is_active() {
            libinput.suspend();
        }
        event_loop
            .insert_source(
                LibinputInputBackend::new(libinput.clone()),
                |mut event, _, state| {
                    state.process_libinput_event(&mut event);
                    state.process_input_event(event);
                },
            )
            .map_err(|err| anyhow!("inserting libinput: {err}"))?;
        event_loop
            .insert_source(notifier, |event, _, state| {
                state
                    .backend
                    .nested()
                    .on_session_event(&mut state.synoik, event);
            })
            .map_err(|err| anyhow!("inserting the session notifier: {err}"))?;

        // The first connection is synchronous: the outputs, and the device to render on, come
        // from the outer, and nothing else can be set up without them.
        let fd = connect(&socket_path)?;
        let (render_node, connectors) = handshake(fd.as_fd())?;
        let render_node = render_node.context("the outer named no render node")?;
        let renderer =
            VulkanRenderer::for_drm_render_node(render_node.0 as u32, render_node.1 as u32)
                .context("creating the Vulkan renderer on the outer's device")?;
        let render_node =
            DrmNode::from_dev_id(rfs::makedev(render_node.0 as u32, render_node.1 as u32)).ok();
        info!(
            "nested in the system compositor at {}: {} display(s)",
            socket_path.display(),
            connectors.len()
        );

        let mut nested = Self {
            session,
            libinput,
            handle: event_loop,
            socket_path,
            conn: None,
            renderer,
            render_node,
            dmabuf_global: None,
            ipc_outputs: Default::default(),
            connectors,
            output_ids: HashMap::new(),
            outputs: HashMap::new(),
            next_buffer_id: 1,
        };
        nested.attach(fd);
        Ok(nested)
    }

    pub fn init(&mut self, synoik: &mut Synoik) {
        self.add_dmabuf_global(synoik);
        self.reconfigure(synoik);
    }

    fn add_dmabuf_global(&mut self, synoik: &mut Synoik) {
        let Some(node) = self.render_node else {
            return;
        };
        let formats = crate::render_helpers::vulkan::dmabuf_formats();
        match DmabufFeedbackBuilder::new(node.dev_id(), formats).build() {
            Ok(feedback) => {
                let global = synoik
                    .dmabuf_state
                    .create_global_with_default_feedback::<State>(
                        &synoik.display_handle,
                        &feedback,
                    );
                self.dmabuf_global = Some(global);
            }
            Err(err) => warn!("error building dmabuf feedback: {err:?}"),
        }
    }

    /// Decide this session's displays — which connectors it lights, at which mode — as the tty
    /// backend does (`Tty::on_output_config_changed`), bring the outputs to that, and tell the
    /// outer, which lights them while this session is the one on screen (§3).
    pub fn reconfigure(&mut self, synoik: &mut Synoik) {
        let wanted = self.decide(synoik);

        let names: Vec<String> = self.outputs.keys().cloned().collect();
        for name in names {
            // A different display on the same connector is a new output, as on the tty
            // (`refresh_changed_identities`): nothing of the old one's state carries over.
            let same = wanted
                .iter()
                .find(|(c, _)| c.connector == name)
                .is_some_and(|(c, _)| c.edid == self.outputs[&name].connector.edid);
            if !same {
                self.remove_output(synoik, &name);
            }
        }
        for (connector, mode) in &wanted {
            match self.outputs.get_mut(&connector.connector) {
                Some(out) if out.mode == *mode => out.connector = connector.clone(),
                Some(_) => self.change_mode(synoik, connector.clone(), *mode),
                None => self.add_output(synoik, connector.clone(), *mode),
            }
        }
        self.refresh_ipc_outputs(synoik);

        let Some(conn) = &self.conn else {
            return;
        };
        let msg = ToOuter::Configure {
            outputs: wanted
                .into_iter()
                .map(|(c, mode)| OutputConfig {
                    connector: c.connector,
                    mode,
                })
                .collect(),
        };
        if let Err(err) = protocol::send(conn.fd.as_fd(), &msg, &[]) {
            warn!("configuring the displays with the system compositor: {err}");
            self.detach(synoik);
        }
    }

    /// The displays to light and their modes: off, mode and lid as the tty backend reads them
    /// from the config, the live-applied display config and `monitors.xml`.
    fn decide(&self, synoik: &mut Synoik) -> Vec<(ConnectorInfo, ModeInfo)> {
        // A live-applied config for a mode the display no longer offers belongs to the display
        // that offered it (`Tty::forget_inapplicable_applied_config`).
        for c in &self.connectors {
            let name = output_name_from_edid(c.connector.clone(), c.edid.as_deref());
            if !synoik.applied_display_config.contains_key(&name.connector) {
                continue;
            }
            let config_mode = synoik
                .config
                .borrow()
                .outputs
                .find(&name)
                .and_then(|c| c.mode);
            if applied_config_is_stale(&advertised(c), config_mode) {
                forget_applied_config(synoik, &name, "its mode is gone");
            }
        }

        let config = synoik.config.borrow();
        // `Tty::should_disable_laptop_panels`: a closed lid turns the panel off when there is
        // another display to use.
        let lid_off = synoik.is_lid_closed
            && !config.debug.keep_laptop_panel_on_when_lid_is_closed
            && self
                .connectors
                .iter()
                .any(|c| !is_laptop_panel(&c.connector));
        let store = MonitorsConfig::load();
        let mut wanted = Vec::new();
        for c in &self.connectors {
            let name = output_name_from_edid(c.connector.clone(), c.edid.as_deref());
            let output = config.outputs.find(&name).cloned().unwrap_or_default();
            if output.off || (lid_off && is_laptop_panel(&c.connector)) {
                continue;
            }
            if output.modeline.is_some() {
                warn!(
                    "output {:?}: a custom modeline cannot be driven nested; \
                     using the advertised modes",
                    c.connector
                );
            }
            let saved: Vec<_> = store
                .as_ref()
                .map(|store| store.saved_modes_for(&name).collect())
                .unwrap_or_default();
            let target = choose_target_mode(&advertised(c), output.mode, &saved);
            if let Some(mode) = pick_mode(&c.modes, target) {
                wanted.push((c.clone(), mode));
            }
        }
        wanted
    }

    fn add_output(&mut self, synoik: &mut Synoik, connector: ConnectorInfo, mode: ModeInfo) {
        let name = output_name_from_edid(connector.connector.clone(), connector.edid.as_deref());
        let output = Output::new(
            connector.connector.clone(),
            PhysicalProperties {
                size: (
                    connector.physical_size_mm.0 as i32,
                    connector.physical_size_mm.1 as i32,
                )
                    .into(),
                subpixel: Subpixel::Unknown,
                make: name.make.clone().unwrap_or_else(|| "Unknown".into()),
                model: name.model.clone().unwrap_or_else(|| "Unknown".into()),
                serial_number: name.serial.clone().unwrap_or_else(|| "Unknown".into()),
            },
        );
        let wl_mode = wl_mode(mode);
        output.change_current_state(Some(wl_mode), None, None, None);
        output.set_preferred(wl_mode);
        output.user_data().insert_if_missing(|| name.clone());

        self.outputs.insert(
            connector.connector.clone(),
            NestedOutput {
                damage_tracker: OutputDamageTracker::from_output(&output),
                output: output.clone(),
                connector,
                mode,
                slots: Vec::new(),
                frame: 0,
                in_flight: None,
                on_screen: None,
                abandoned: None,
            },
        );
        synoik.add_output(output, Some(refresh(mode)), false);
    }

    fn remove_output(&mut self, synoik: &mut Synoik, name: &str) {
        let Some(mut out) = self.outputs.remove(name) else {
            return;
        };
        self.free_slots(&mut out);
        synoik.remove_output(&out.output);
    }

    /// The display runs at another mode now: everything sized for the old one goes.
    fn change_mode(&mut self, synoik: &mut Synoik, connector: ConnectorInfo, mode: ModeInfo) {
        let Some(mut out) = self.outputs.remove(&connector.connector) else {
            return;
        };
        let slot_ids: Vec<u32> = out.slots.iter().map(|s| s.id).collect();
        self.free_slots(&mut out);
        // The outer answers the frame in flight as the old mode's last; that answer finds nothing
        // in flight, so the redraw machinery is released here instead.
        out.abandoned = None;
        if let Some((index, _, _)) = out.in_flight.take() {
            out.abandoned = Some(slot_ids[index]);
            if let Some(state) = synoik.output_state.get_mut(&out.output) {
                state.redraw_state = RedrawState::Idle;
            }
        }
        out.on_screen = None;
        out.frame = 0;
        out.mode = mode;
        out.connector = connector;

        let output = out.output.clone();
        let wl_mode = wl_mode(mode);
        output.change_current_state(Some(wl_mode), None, None, None);
        output.set_preferred(wl_mode);
        out.damage_tracker = OutputDamageTracker::from_output(&output);
        self.outputs.insert(out.connector.connector.clone(), out);

        if let Some(state) = synoik.output_state.get_mut(&output) {
            state.frame_clock = FrameClock::new(Some(refresh(mode)), false);
        }
        // Scale and transform are keyed on the mode (`Tty::on_output_config_changed`).
        let monitors_config = MonitorsConfig::load();
        let (scale, transform) =
            synoik.derive_output_scale_transform(&output, monitors_config.as_ref());
        output.change_current_state(
            None,
            Some(transform),
            Some(smithay::output::Scale::Fractional(scale)),
            None,
        );
        synoik.output_resized(&output);
        synoik.queue_redraw(&output);
    }

    /// Give back an output's scanout buffers. The outer keeps a removed buffer's framebuffer for
    /// as long as it is on screen.
    fn free_slots(&mut self, out: &mut NestedOutput) {
        let conn = self.conn.as_ref().map(|c| c.fd.clone());
        for slot in out.slots.drain(..) {
            if let Some(conn) = &conn {
                if let Err(err) =
                    protocol::send(conn.as_fd(), &ToOuter::RemoveBuffer { id: slot.id }, &[])
                {
                    warn!("removing buffer {}: {err}", slot.id);
                }
            }
        }
    }

    /// Every connected display for IPC and `DisplayConfig`, lit or not: an output this session
    /// turned off is still one Settings can turn back on.
    fn refresh_ipc_outputs(&mut self, synoik: &mut Synoik) {
        self.output_ids
            .retain(|name, _| self.connectors.iter().any(|c| c.connector == *name));
        let mut map = IpcOutputMap::new();
        for c in &self.connectors {
            let name = output_name_from_edid(c.connector.clone(), c.edid.as_deref());
            let id = *self
                .output_ids
                .entry(c.connector.clone())
                .or_insert_with(OutputId::next);
            let lit = self.outputs.get(&c.connector);
            map.insert(
                id,
                synoik_ipc::Output {
                    name: c.connector.clone(),
                    make: name.make.clone().unwrap_or_else(|| "Unknown".into()),
                    vendor: name.vendor.clone(),
                    model: name.model.clone().unwrap_or_else(|| "Unknown".into()),
                    serial: name.serial.clone(),
                    physical_size: Some(c.physical_size_mm),
                    modes: c
                        .modes
                        .iter()
                        .map(|m| synoik_ipc::Mode {
                            width: m.width,
                            height: m.height,
                            refresh_rate: m.refresh_mhz,
                            is_preferred: m.preferred,
                        })
                        .collect(),
                    current_mode: lit.and_then(|o| c.modes.iter().position(|m| *m == o.mode)),
                    is_custom_mode: false,
                    vrr_supported: false,
                    vrr_enabled: false,
                    // `State::refresh_ipc_outputs` fills it in from the space.
                    logical: None,
                    max_bpc: None,
                },
            );
        }
        *self.ipc_outputs.lock().unwrap() = map;
        synoik.ipc_outputs_changed = true;
    }

    fn on_session_event(&mut self, synoik: &mut Synoik, event: SessionEvent) {
        match event {
            SessionEvent::PauseSession => {
                debug!("session paused");
                self.libinput.suspend();
            }
            SessionEvent::ActivateSession => {
                debug!("session activated");
                if self.libinput.resume().is_err() {
                    warn!("error resuming libinput");
                }
                synoik.notify_activity();
                synoik.queue_redraw_all();
            }
        }
    }

    /// Start listening on a connection that has completed its handshake.
    fn attach(&mut self, fd: OwnedFd) {
        let fd = Rc::new(fd);
        let source = FdRef(fd.clone());
        let token = self
            .handle
            .insert_source(
                Generic::new(source, Interest::READ, CalloopMode::Level),
                |_, _, state| {
                    state.backend.nested().on_readable(&mut state.synoik);
                    Ok(PostAction::Continue)
                },
            )
            .expect("inserting the outer's connection");
        self.conn = Some(Connection { fd, token });
    }

    fn on_readable(&mut self, synoik: &mut Synoik) {
        loop {
            let Some(conn) = &self.conn else {
                return;
            };
            let fd = conn.fd.clone();
            match protocol::recv::<ToInner>(fd.as_fd()) {
                Ok(Received::Message(msg, _fds)) => self.on_message(synoik, msg),
                Ok(Received::WouldBlock) => return,
                Ok(Received::Closed) => {
                    warn!("the system compositor went away");
                    self.detach(synoik);
                    return;
                }
                Err(err) => {
                    warn!("reading from the system compositor: {err}");
                    self.detach(synoik);
                    return;
                }
            }
        }
    }

    fn on_message(&mut self, synoik: &mut Synoik, msg: ToInner) {
        match msg {
            ToInner::Welcome { .. } => warn!("unexpected Welcome outside the handshake"),
            ToInner::Connectors { connectors } => {
                self.connectors = connectors;
                self.reconfigure(synoik);
            }
            ToInner::Presented {
                output,
                buffer,
                sequence,
                time_ns,
                refresh_ns,
            } => self.on_presented(synoik, &output, buffer, sequence, time_ns, refresh_ns),
            ToInner::Discarded { output, buffer } => self.on_discarded(synoik, &output, buffer),
        }
    }

    /// The outer is gone. Forget everything it was holding for us and start knocking.
    fn detach(&mut self, synoik: &mut Synoik) {
        // Both a failed send and the read side can notice the same death; only the first one
        // starts the reconnect loop.
        let Some(conn) = self.conn.take() else {
            return;
        };
        self.handle.remove(conn.token);
        for out in self.outputs.values_mut() {
            out.on_screen = None;
            // The frame in flight will never be answered; finish its bookkeeping as a frame that
            // did not present, so the redraw machinery is not left waiting for a vblank.
            if out.in_flight.take().is_some() {
                if let Some(state) = synoik.output_state.get_mut(&out.output) {
                    state.redraw_state = RedrawState::Idle;
                }
            }
            for slot in &mut out.slots {
                slot.drawn = 0;
            }
        }
        schedule_reconnect(synoik);
    }

    fn try_reconnect(&mut self, synoik: &mut Synoik) -> bool {
        let fd = match connect(&self.socket_path) {
            Ok(fd) => fd,
            Err(err) => {
                trace!("reconnecting to the system compositor: {err:#}");
                return false;
            }
        };
        let connectors = match handshake(fd.as_fd()) {
            Ok((_, connectors)) => connectors,
            Err(err) => {
                warn!("reattaching to the system compositor: {err:#}");
                return false;
            }
        };
        info!("reattached to the system compositor");
        self.attach(fd);

        // Everything the old outer had imported died with it.
        let conn = self.conn.as_ref().expect("just attached").fd.clone();
        for out in self.outputs.values() {
            for slot in &out.slots {
                if let Err(err) = send_add_buffer(conn.as_fd(), slot) {
                    warn!("re-registering buffer {}: {err}", slot.id);
                }
            }
        }
        // The displays may have changed while it was gone; this also tells it the layout.
        self.connectors = connectors;
        self.reconfigure(synoik);
        synoik.queue_redraw_all();
        true
    }

    pub fn seat_name(&self) -> String {
        self.session.seat()
    }

    pub fn with_vulkan_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut VulkanRenderer) -> T,
    ) -> Option<T> {
        Some(f(&mut self.renderer))
    }

    pub fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            warn!("error changing VT: {err}");
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        match self.renderer.import_dmabuf(dmabuf, None) {
            Ok(_) => true,
            Err(err) => {
                debug!("error importing dmabuf: {err:?}");
                false
            }
        }
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }

    #[cfg(feature = "xdp-gnome-screencast")]
    pub fn gbm_device(
        &mut self,
    ) -> Option<smithay::backend::allocator::gbm::GbmDevice<smithay::backend::drm::DrmDeviceFd>>
    {
        let path = self.render_node?.dev_path()?;
        let flags = rfs::OFlags::RDWR | rfs::OFlags::CLOEXEC | rfs::OFlags::NOCTTY;
        let fd = rfs::open(&path, flags, rfs::Mode::empty()).ok()?;
        smithay::backend::allocator::gbm::GbmDevice::new(smithay::backend::drm::DrmDeviceFd::new(
            smithay::utils::DeviceFd::from(fd),
        ))
        .ok()
    }

    pub fn render(
        &mut self,
        synoik: &mut Synoik,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        let _span = tracy_client::span!("Nested::render");

        // A greeter whose dialog is fading out for a new session draws on in the background: logind
        // fronted that session when it was created, before the fade began (a VT-less seat
        // activates a session as it attaches), and the outer keeps the greeter's frames on screen
        // until the session has one of its own. It shows nothing private.
        let fading_greeter = synoik.greeter && synoik.lock_screen.dialog_hidden();
        if self.conn.is_none() || !(self.session.is_active() || fading_greeter) {
            // Nowhere to show it. Pace off the clock until the outer is back or the session is
            // fronted again, the same as a tty frame that failed to queue.
            queue_estimated_vblank_timer(synoik, output.clone());
            return RenderResult::Skipped;
        }

        let name = output.name();
        let Some(out) = self.outputs.get_mut(&name) else {
            error!("render on an output the outer never described: {name}");
            return RenderResult::Skipped;
        };
        if out.in_flight.is_some() {
            // The redraw state machine does not render with a frame in flight; if it ever does,
            // the outer's single-flip rule would drop this one anyway.
            error!("render on {name} with a frame in flight");
            return RenderResult::Skipped;
        }

        let size = Size::from((i32::from(out.mode.width), i32::from(out.mode.height)));
        if out.slots.is_empty() {
            let conn = self.conn.as_ref().expect("checked above").fd.clone();
            if let Err(err) = allocate_slots(
                &self.renderer,
                self.render_node,
                &mut self.next_buffer_id,
                out,
                size,
                conn.as_fd(),
            ) {
                warn!("allocating scanout buffers for {name}: {err:#}");
                queue_estimated_vblank_timer(synoik, output.clone());
                return RenderResult::Skipped;
            }
        }

        // Any slot but the one being scanned out, oldest first: the oldest has the most damage
        // owed, but also the only buffer guaranteed not to be on screen once two have flipped.
        let Some(index) = (0..out.slots.len())
            .filter(|&i| Some(i) != out.on_screen)
            .min_by_key(|&i| out.slots[i].drawn)
        else {
            return RenderResult::Skipped;
        };

        let ctx = RenderCtx {
            renderer: &mut self.renderer,
            target: RenderTarget::Output,
            appearance: Some(synoik.appearance()),
        };
        let elements = synoik.render_to_vec(ctx, output, true);

        out.frame += 1;
        let slot = &mut out.slots[index];
        let age = match slot.drawn {
            0 => 0,
            last => (out.frame - last) as usize,
        };

        // The fence of this frame has somewhere to go — the outer hands it to the kernel as
        // `IN_FENCE_FD` — so the renderer may leave the submit in flight, as on the tty.
        self.renderer.set_finish_may_defer(true);
        let rendered = match self.renderer.bind(&mut slot.dmabuf) {
            Ok(mut fb) => out.damage_tracker.render_output(
                &mut self.renderer,
                &mut fb,
                age,
                &elements,
                [0., 0., 0., 1.],
            ),
            Err(err) => {
                self.renderer.set_finish_may_defer(false);
                warn!("binding a scanout buffer: {err:?}");
                queue_estimated_vblank_timer(synoik, output.clone());
                return RenderResult::Skipped;
            }
        };
        self.renderer.set_finish_may_defer(false);

        let res = match rendered {
            Ok(res) => res,
            Err(err) => {
                warn!("rendering a frame for {name}: {err:?}");
                queue_estimated_vblank_timer(synoik, output.clone());
                return RenderResult::Skipped;
            }
        };
        synoik.update_primary_scanout_output(output, &res.states);

        if res.damage.is_none() {
            // Nothing changed: the slot's contents are what they were, so do not mark it drawn,
            // and do not bother the outer with a frame identical to the one it shows.
            out.frame -= 1;
            queue_estimated_vblank_timer(synoik, output.clone());
            return RenderResult::NoDamage;
        }
        slot.drawn = out.frame;
        let buffer = slot.id;

        let fence = res.sync.export();
        if fence.is_none() {
            // A sync point that cannot be exported is either already signalled or one the outer
            // cannot wait on; waiting here makes both correct.
            if let Err(err) = res.sync.wait() {
                warn!("waiting for a frame that could not hand over its fence: {err:?}");
            }
        }
        let msg = ToOuter::Present {
            output: name.clone(),
            buffer,
            fenced: fence.is_some(),
        };
        let conn = self.conn.as_ref().expect("checked above").fd.clone();
        let fds: Vec<BorrowedFd<'_>> = fence.iter().map(|fd| fd.as_fd()).collect();
        if let Err(err) = protocol::send(conn.as_fd(), &msg, &fds) {
            warn!("presenting to the system compositor: {err}");
            self.detach(synoik);
            queue_estimated_vblank_timer(synoik, output.clone());
            return RenderResult::Skipped;
        }

        let feedback = synoik.take_presentation_feedbacks(output, &res.states);
        let out = self.outputs.get_mut(&name).expect("looked up above");
        out.in_flight = Some((index, feedback, target_presentation_time));
        synoik
            .frame_log
            .queued(&name, target_presentation_time, get_monotonic_time());

        let output_state = synoik.output_state.get_mut(output).unwrap();
        match mem::replace(
            &mut output_state.redraw_state,
            RedrawState::WaitingForVBlank {
                redraw_needed: false,
            },
        ) {
            RedrawState::Queued => (),
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => synoik.cancel_timer(token),
            state => unreachable!("render in redraw state {state:?}"),
        }
        let output_state = synoik.output_state.get_mut(output).unwrap();
        // The client buffers this frame sampled are latched; a new commit waits for the flip.
        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        RenderResult::Submitted
    }

    /// The outer flipped our frame. The vblank of the tty backend, with the kernel's timestamp
    /// carried across the socket untouched.
    fn on_presented(
        &mut self,
        synoik: &mut Synoik,
        name: &str,
        buffer: u32,
        sequence: u32,
        time_ns: u64,
        refresh_ns: u64,
    ) {
        let Some(out) = self.outputs.get_mut(name) else {
            return;
        };
        // Only the frame in flight can be answered. Taking another buffer's `Presented` for it
        // would mark the wrong slot on screen and free the one that is.
        let Some(index) = out
            .in_flight
            .as_ref()
            .map(|(index, _, _)| *index)
            .filter(|&index| out.slots.get(index).is_some_and(|s| s.id == buffer))
        else {
            if out.abandoned.take() == Some(buffer) {
                debug!("Presented buffer {buffer} on {name}, of the mode it ran before");
            } else {
                warn!("Presented buffer {buffer} on {name}, which is not in flight");
            }
            return;
        };
        let (_, mut feedback, target) = out.in_flight.take().expect("checked above");
        out.on_screen = Some(index);
        let output = out.output.clone();

        let now = get_monotonic_time();
        let presentation_time = Duration::from_nanos(time_ns);
        let time = if presentation_time.is_zero() {
            now
        } else {
            presentation_time
        };

        let Some(output_state) = synoik.output_state.get_mut(&output) else {
            return;
        };
        let redraw_needed = match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
            state => {
                error!("Presented on {name} in redraw state {state:?}");
                true
            }
        };

        let refresh = if refresh_ns == 0 {
            Refresh::Unknown
        } else {
            Refresh::Fixed(Duration::from_nanos(refresh_ns))
        };
        let mut flags =
            wp_presentation_feedback::Kind::Vsync | wp_presentation_feedback::Kind::HwCompletion;
        if !presentation_time.is_zero() {
            flags.insert(wp_presentation_feedback::Kind::HwClock);
        }
        feedback.presented::<_, smithay::utils::Monotonic>(
            time,
            refresh,
            u64::from(sequence),
            flags,
        );

        output_state.frame_clock.presented(presentation_time);
        let shield_frame_presented = mem::take(&mut output_state.shield_frame_queued);
        let unfinished_animations_remain = output_state.unfinished_animations_remain;
        let anim_causes = output_state.last_frame_anim_causes;
        let refresh_interval = output_state.frame_clock.refresh_interval();

        synoik
            .frame_log
            .presented(name, target, presentation_time, refresh_interval);
        if shield_frame_presented {
            synoik.note_shield_frame_presented(&output);
        }

        if unfinished_animations_remain {
            synoik.queue_redraw_for(&output, crate::frame_log::Requester::Animation(anim_causes));
        } else if redraw_needed {
            synoik.queue_deferred_redraw(&output);
        } else {
            synoik.send_frame_callbacks(&output);
        }
    }

    /// The outer will not show the frame in flight — the flip was refused, or a newer frame
    /// superseded it while this session was in the background. Its slot is free again, and an
    /// active session owes a fresh frame.
    fn on_discarded(&mut self, synoik: &mut Synoik, name: &str, buffer: u32) {
        let Some(out) = self.outputs.get_mut(name) else {
            return;
        };
        // Only the frame in flight can be discarded; with nothing in flight the redraw machinery
        // is in some other state, which forcing it idle would corrupt.
        let Some(index) = out
            .in_flight
            .as_ref()
            .map(|(index, _, _)| *index)
            .filter(|&index| out.slots.get(index).is_some_and(|s| s.id == buffer))
        else {
            if out.abandoned.take() == Some(buffer) {
                debug!("Discarded buffer {buffer} on {name}, of the mode it ran before");
            } else {
                warn!("Discarded buffer {buffer} on {name}, which is not in flight");
            }
            return;
        };
        out.in_flight = None;
        // Its contents never reached the screen, so the damage history it was drawn against is
        // not one anything will be compared with.
        out.slots[index].drawn = 0;
        let output = out.output.clone();
        if let Some(state) = synoik.output_state.get_mut(&output) {
            state.redraw_state = RedrawState::Idle;
        }
        // A session in the background is discarded because it is not on screen; redrawing would
        // only be discarded again. Activation redraws everything (`on_session_event`).
        if self.session.is_active() {
            synoik.queue_redraw(&output);
        }
    }

    pub(super) fn on_estimated_vblank_timer(&self, synoik: &mut Synoik, output: Output) {
        let Some(output_state) = synoik.output_state.get_mut(&output) else {
            return;
        };
        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);
        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::WaitingForEstimatedVBlank(_) => (),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                output_state.redraw_state = RedrawState::Queued;
                return;
            }
            state => unreachable!("estimated vblank in redraw state {state:?}"),
        }
        if output_state.unfinished_animations_remain {
            let causes = output_state.last_frame_anim_causes;
            synoik.queue_redraw_for(&output, crate::frame_log::Requester::Animation(causes));
        } else {
            synoik.send_frame_callbacks(&output);
        }
    }
}

fn wl_mode(mode: ModeInfo) -> Mode {
    Mode {
        size: (i32::from(mode.width), i32::from(mode.height)).into(),
        refresh: mode.refresh_mhz as i32,
    }
}

fn refresh(mode: ModeInfo) -> Duration {
    Duration::from_nanos(1_000_000_000_000 / u64::from(mode.refresh_mhz.max(1)))
}

fn advertised(c: &ConnectorInfo) -> Vec<AdvertisedMode> {
    c.modes
        .iter()
        .map(|m| AdvertisedMode {
            width: m.width,
            height: m.height,
            refresh: m.refresh_mhz as i32,
        })
        .collect()
}

/// `pick_mode` of the tty backend over the outer's mode list: the target's size at its refresh,
/// or that size's fastest; else the fastest preferred mode; else the first.
fn pick_mode(modes: &[ModeInfo], target: Option<synoik_config::output::Mode>) -> Option<ModeInfo> {
    if let Some(target) = target.filter(|t| !t.custom) {
        let refresh = target.mode.refresh.map(|r| (r * 1000.).round() as u32);
        let found = modes
            .iter()
            .filter(|m| (m.width, m.height) == (target.mode.width, target.mode.height))
            .filter(|m| refresh.is_none_or(|r| m.refresh_mhz == r))
            .max_by_key(|m| m.refresh_mhz);
        if found.is_some() {
            return found.copied();
        }
        warn!(
            "configured mode {}x{} could not be found, falling back to preferred",
            target.mode.width, target.mode.height
        );
    }
    modes
        .iter()
        .filter(|m| m.preferred)
        .max_by_key(|m| m.refresh_mhz)
        .or(modes.first())
        .copied()
}

/// Knock on the outer's door until it answers, on the compositor's clock.
fn schedule_reconnect(synoik: &mut Synoik) {
    synoik.timer_after(RECONNECT_INTERVAL, |state| {
        if state.backend.nested().try_reconnect(&mut state.synoik) {
            None
        } else {
            Some(RECONNECT_INTERVAL)
        }
    });
}

/// The same estimated-vblank pacer the tty backend runs when a frame does not reach the screen.
fn queue_estimated_vblank_timer(synoik: &mut Synoik, output: Output) {
    let output_state = synoik.output_state.get_mut(&output).unwrap();
    match mem::take(&mut output_state.redraw_state) {
        RedrawState::Queued => (),
        RedrawState::WaitingForEstimatedVBlank(token)
        | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
            output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
            return;
        }
        state => unreachable!("estimated vblank timer in redraw state {state:?}"),
    }

    let now = get_monotonic_time();
    let deadline = output_state.frame_clock.estimated_vblank_deadline(now);
    let timer_output = output.clone();
    let token = synoik.timer_at(deadline, move |state| {
        state
            .backend
            .nested()
            .on_estimated_vblank_timer(&mut state.synoik, timer_output.clone());
        None
    });
    let output_state = synoik.output_state.get_mut(&output).unwrap();
    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
}

/// Allocate an output's scanout buffers on our device and register each with the outer.
fn allocate_slots(
    renderer: &VulkanRenderer,
    render_node: Option<DrmNode>,
    next_id: &mut u32,
    out: &mut NestedOutput,
    size: Size<i32, smithay::utils::Physical>,
    conn: BorrowedFd<'_>,
) -> anyhow::Result<()> {
    let mut allocator = VulkanScanoutAllocator::new(renderer.gpu().clone(), render_node)?;
    for _ in 0..SLOTS {
        // XRGB8888, LINEAR: what the virtio-gpu primary plane takes and what our renderer draws
        // straight into (`matches_render_order`); LINEAR because that plane cannot be told any
        // other layout (`device_names_modifiers`).
        let buffer = allocator
            .create_buffer(
                size.w as u32,
                size.h as u32,
                Fourcc::Xrgb8888,
                &[Modifier::Linear],
            )
            .map_err(|err| anyhow!("{err}"))?;
        let dmabuf = buffer.export().context("exporting a scanout buffer")?;
        let slot = Slot {
            id: *next_id,
            _buffer: buffer,
            dmabuf,
            drawn: 0,
        };
        *next_id += 1;
        send_add_buffer(conn, &slot)?;
        out.slots.push(slot);
    }
    Ok(())
}

fn send_add_buffer(conn: BorrowedFd<'_>, slot: &Slot) -> anyhow::Result<()> {
    let dmabuf = &slot.dmabuf;
    let fd = dmabuf.handles().next().context("a dmabuf with no planes")?;
    let msg = ToOuter::AddBuffer {
        id: slot.id,
        width: dmabuf.width(),
        height: dmabuf.height(),
        fourcc: dmabuf.format().code as u32,
        modifier: u64::from(dmabuf.format().modifier),
        stride: dmabuf.strides().next().unwrap_or(0),
        offset: dmabuf.offsets().next().unwrap_or(0),
    };
    protocol::send(conn, &msg, &[fd])?;
    Ok(())
}

/// Connect to the outer's socket. Non-blocking from here on; the handshake polls.
fn connect(path: &Path) -> anyhow::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;

    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("socket");
    }
    // SAFETY: just created.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        bail!("socket path too long: {}", path.display());
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let rv = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_un>() as _,
        )
    };
    if rv < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("connecting to {}", path.display()));
    }
    Ok(fd)
}

/// The logind session this inner belongs to, the way mutter finds its own: the session scope the
/// process runs in, else the uid's graphical session (`sd_pid_get_session`, then
/// `sd_uid_get_display`, read from logind's state files as libsystemd does). A session compositor
/// started as a user service — the greeter, under gnome-session — runs in the user manager, outside
/// any scope, and its environment has no `XDG_SESSION_ID`: gnome-session does not export one into
/// a manager every session of the uid shares.
fn own_session_id() -> Option<String> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        return Some(id);
    }
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let scope = cgroup
        .split('/')
        .find_map(|part| part.strip_prefix("session-")?.strip_suffix(".scope"))
        .map(|id| id.trim().to_owned());
    scope.or_else(|| {
        // SAFETY: `getuid` is always successful and touches no memory.
        let uid = unsafe { libc::getuid() };
        let state = std::fs::read_to_string(format!("/run/systemd/users/{uid}")).ok()?;
        state
            .lines()
            .find_map(|line| line.strip_prefix("DISPLAY="))
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
    })
}

/// What the outer's `Welcome` gives a session: its render node and the connected displays.
type Welcomed = (Option<(u64, u64)>, Vec<ConnectorInfo>);

/// Say hello and wait for the welcome. Bounded, so a wedged outer fails startup instead of hanging
/// it.
fn handshake(fd: BorrowedFd<'_>) -> anyhow::Result<Welcomed> {
    let hello = ToOuter::Hello {
        version: protocol::VERSION,
        session: own_session_id(),
    };
    protocol::send(fd, &hello, &[])?;

    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 5000) } <= 0 {
        bail!("no Welcome from the system compositor within 5 s");
    }
    match protocol::recv::<ToInner>(fd)? {
        Received::Message(
            ToInner::Welcome {
                version,
                render_node,
                connectors,
            },
            _,
        ) => {
            if version != protocol::VERSION {
                bail!(
                    "the system compositor speaks protocol {version}, this session {}",
                    protocol::VERSION
                );
            }
            Ok((render_node, connectors))
        }
        Received::Message(other, _) => bail!("expected Welcome, got {other:?}"),
        Received::WouldBlock | Received::Closed => {
            bail!("the system compositor closed the connection during the handshake")
        }
    }
}

/// An `Rc<OwnedFd>` the event loop can poll while the backend keeps its own reference for sends.
struct FdRef(Rc<OwnedFd>);

impl AsFd for FdRef {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: u16, height: u16, refresh_mhz: u32, preferred: bool) -> ModeInfo {
        ModeInfo {
            width,
            height,
            refresh_mhz,
            preferred,
        }
    }

    fn target(width: u16, height: u16, refresh: Option<f64>) -> synoik_config::output::Mode {
        synoik_config::output::Mode {
            custom: false,
            mode: synoik_ipc::ConfiguredMode {
                width,
                height,
                refresh,
            },
        }
    }

    /// The tty backend's rules, over the outer's list.
    #[test]
    fn a_mode_is_picked_as_the_tty_picks_it() {
        let modes = [
            mode(2560, 1600, 60_000, true),
            mode(1920, 1080, 60_000, false),
            mode(1920, 1080, 120_000, false),
            mode(2560, 1600, 120_000, true),
        ];
        // No target: the fastest preferred mode.
        assert_eq!(pick_mode(&modes, None), Some(modes[3]));
        // A size without a rate: that size's fastest.
        assert_eq!(
            pick_mode(&modes, Some(target(1920, 1080, None))),
            Some(modes[2])
        );
        // A rate narrows it.
        assert_eq!(
            pick_mode(&modes, Some(target(1920, 1080, Some(60.)))),
            Some(modes[1])
        );
        // A mode the display does not offer falls back to preferred.
        assert_eq!(
            pick_mode(&modes, Some(target(800, 600, None))),
            Some(modes[3])
        );
        // No preferred mode at all: the first.
        assert_eq!(
            pick_mode(&[mode(1024, 768, 60_000, false)], None),
            Some(mode(1024, 768, 60_000, false))
        );
    }
}
