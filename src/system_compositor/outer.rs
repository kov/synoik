// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The system compositor: holds DRM master for the machine and scans a nested session out
//! (`docs/fork/system-compositor.md`).
//!
//! Deliberately not a `Synoik`. In steady state it composites nothing — it flips the fronted
//! session's own buffers — so it is a small event loop over the DRM device and one socket, and
//! that thinness is what keeps restart and reattach cheap (§1, §7).
//!
//! What this cut does: master and modeset of the first connected display at its preferred mode,
//! a plate until a session attaches, then one nested session unredirected, with the session's
//! render fence handed to the commit as `IN_FENCE_FD` and the kernel's flip timestamps passed
//! back. The DRM fd lives in systemd's fd store, so a restarted outer is still master and the
//! session's last frame stays on screen across the gap (§1, §7).

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{bail, Context as _};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Fourcc, Modifier};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, DrmSurface,
    NodeType, PlaneConfig, PlaneState,
};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::drm::buffer::DrmFourcc;
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::{connector, crtc, framebuffer, Device as _, ModeTypeFlags};
use smithay::reexports::rustix::fs as rfs;
use smithay::utils::{DeviceFd, Rectangle, Transform};

use super::protocol::{self, ModeInfo, OutputInfo, Received, ToInner, ToOuter};
use crate::backend::tty::{edid_blob, format_connector_name, refresh_interval};
use crate::backend::vulkan_scanout::{framebuffer_from_dmabuf, PrimeFramebuffer};

/// The name the DRM fd is stored under in systemd's fd store.
const FDSTORE_NAME: &str = "drm";

/// Run the system compositor until it is told to stop.
pub fn run(card: Option<PathBuf>, socket: PathBuf) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<'static, Outer> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    let (fd, recovered) = acquire_card(card.as_deref())?;
    if recovered {
        close_inherited_framebuffers(fd.as_fd());
    }
    let mut outer = Outer::new(fd, recovered, handle.clone())?;

    let listener = listen(&socket)?;
    handle
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            |_, listener, outer: &mut Outer| {
                outer.accept(listener.as_fd());
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| anyhow::anyhow!("inserting the listener: {err}"))?;
    info!("system compositor listening on {}", socket.display());

    notify(&[sd_notify::NotifyState::Ready]);
    event_loop.run(None, &mut outer, |_| ())?;
    Ok(())
}

/// The DRM fd: the one systemd kept for us if this is a restart, else a fresh open, which is then
/// handed to systemd to keep.
///
/// Master belongs to the open file, not to us. Holding the file in systemd's fd store is what
/// stops a crash from leaving a masterless card for anyone with a `uaccess` ACL to claim (§1).
///
/// Returns whether the fd was recovered rather than freshly opened.
fn acquire_card(card: Option<&Path>) -> anyhow::Result<(OwnedFd, bool)> {
    if let Ok(fds) = sd_notify::listen_fds_with_names() {
        for (raw, name) in fds {
            // SAFETY: systemd passed this descriptor to us; it is ours to own from here on.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            if name == FDSTORE_NAME {
                info!("recovered the DRM fd from the fd store");
                return Ok((fd, true));
            }
        }
    }

    let path = match card {
        Some(path) => path.to_owned(),
        None => PathBuf::from("/dev/dri/card0"),
    };
    let flags = rfs::OFlags::RDWR | rfs::OFlags::CLOEXEC | rfs::OFlags::NOCTTY;
    let fd = rfs::open(&path, flags, rfs::Mode::empty())
        .with_context(|| format!("opening {}", path.display()))?;
    info!("opened {}", path.display());

    // Best effort: outside systemd there is no store, and that is a supported way to run this by
    // hand. Under the unit, failing to store it is worth a warning, not a refusal to start.
    if std::env::var_os("NOTIFY_SOCKET").is_some() {
        if let Err(err) = sd_notify::notify_with_fds(
            &[
                sd_notify::NotifyState::FdStore,
                sd_notify::NotifyState::FdName(FDSTORE_NAME),
            ],
            &[fd.as_fd()],
        ) {
            warn!("could not put the DRM fd in the fd store: {err}");
        }
    }
    Ok((fd, false))
}

/// Let go of the framebuffers a previous outer created on the fd we inherited, without taking
/// them off the screen.
///
/// Framebuffers belong to the open file, so a restarted outer inherits every one its predecessor
/// made — including the session frame still being scanned out, which is the whole point of keeping
/// the file (§7). It cannot use them (it does not know which buffer each one was) and must not
/// `RMFB` them: removing the framebuffer a plane is showing turns the plane off, a blank screen
/// across every restart. `CLOSEFB` (Linux 6.8) is the handover ioctl for exactly this: the handle
/// goes, the scanout stays until the next flip replaces it.
///
/// Doing it first is also what lets the DRM device be built at all: smithay snapshots the
/// properties of every framebuffer on the file, and framebuffers have none, so an inherited one
/// fails device creation with `EINVAL`.
fn close_inherited_framebuffers(fd: BorrowedFd<'_>) {
    use smithay::reexports::drm::control::Device as ControlDevice;

    struct Card<'a>(BorrowedFd<'a>);
    impl AsFd for Card<'_> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0
        }
    }
    impl smithay::reexports::drm::Device for Card<'_> {}
    impl ControlDevice for Card<'_> {}

    let card = Card(fd);
    let fbs = match card.resource_handles() {
        Ok(res) => res.framebuffers().to_vec(),
        Err(err) => {
            warn!("listing inherited framebuffers: {err}");
            return;
        }
    };
    for fb in fbs {
        #[repr(C)]
        struct DrmModeClosefb {
            fb_id: u32,
            pad: u32,
        }
        // DRM_IOWR(0xD0, struct drm_mode_closefb)
        const DRM_IOCTL_MODE_CLOSEFB: libc::c_ulong = 0xC008_64D0;
        let mut arg = DrmModeClosefb {
            fb_id: u32::from(fb),
            pad: 0,
        };
        if unsafe { libc::ioctl(fd.as_raw_fd(), DRM_IOCTL_MODE_CLOSEFB, &raw mut arg) } < 0 {
            warn!(
                "closing inherited framebuffer {fb:?}: {}",
                io::Error::last_os_error()
            );
        }
    }
}

fn notify(state: &[sd_notify::NotifyState<'_>]) {
    if std::env::var_os("NOTIFY_SOCKET").is_some() {
        if let Err(err) = sd_notify::notify(state) {
            warn!("sd_notify: {err}");
        }
    }
}

/// A `SOCK_SEQPACKET` listener at `path`, replacing whatever a previous run left there.
fn listen(path: &Path) -> anyhow::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;

    let _ = std::fs::remove_file(path);
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error()).context("socket");
    }
    // SAFETY: just created, owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        bail!("socket path too long: {}", path.display());
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let rv = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_un>() as _,
        )
    };
    if rv < 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("bind {}", path.display()));
    }
    // Anyone may connect; who may *attach* is decided per connection from the peer's credentials
    // (`peer_may_attach`), which a mode bit cannot express.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666))
        .with_context(|| format!("chmod {}", path.display()))?;
    if unsafe { libc::listen(fd.as_raw_fd(), 4) } < 0 {
        return Err(io::Error::last_os_error()).context("listen");
    }
    Ok(fd)
}

/// What is on (or on its way to) the screen.
enum Shown {
    Plate,
    Session {
        buffer: u32,
        /// Held so the framebuffer outlives a `RemoveBuffer` while KMS still scans it out:
        /// `RMFB` on the framebuffer a CRTC is showing turns the CRTC off.
        _fb: Rc<PrimeFramebuffer>,
        generation: u64,
    },
}

/// The display this cut drives: one connector, one CRTC.
struct Display {
    connector_name: String,
    surface: DrmSurface,
    mode: smithay::reexports::drm::control::Mode,
    info: OutputInfo,
    refresh: Duration,
    /// Kept alive for as long as the plate may be shown.
    _plate_buffer: DumbBuffer,
    plate: framebuffer::Handle,
}

/// The attached session.
struct Session {
    fd: Rc<OwnedFd>,
    token: RegistrationToken,
    /// Bumped per connection, so a flip that completes after a session left is not reported to
    /// the next one.
    generation: u64,
    welcomed: bool,
    buffers: HashMap<u32, Rc<PrimeFramebuffer>>,
}

struct Outer {
    handle: LoopHandle<'static, Outer>,
    drm_fd: DrmDeviceFd,
    _drm: DrmDevice,
    display: Display,
    render_node: Option<(u64, u64)>,
    session: Option<Session>,
    next_generation: u64,
    /// The last frame whose flip completed.
    on_screen: Shown,
    /// The flip in flight, if any. One at a time: the kernel refuses a second flip on a CRTC with
    /// one pending.
    in_flight: Option<Shown>,
    /// A present that arrived while a flip was in flight; latest wins.
    queued: Option<(u32, Option<OwnedFd>)>,
}

impl Outer {
    fn new(
        fd: OwnedFd,
        recovered: bool,
        handle: LoopHandle<'static, Outer>,
    ) -> anyhow::Result<Self> {
        let drm_fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let render_node = DrmNode::from_file(drm_fd.as_fd())
            .ok()
            .and_then(|node| node.node_with_type(NodeType::Render).and_then(Result::ok))
            .map(|node| (u64::from(node.major()), u64::from(node.minor())));

        // `false`: a restarted outer must not blank the session frame still on screen.
        let (mut drm, notifier) =
            DrmDevice::new(drm_fd.clone(), false).context("creating the DRM device")?;
        handle
            .insert_source(notifier, |event, meta, outer: &mut Outer| match event {
                DrmEvent::VBlank(crtc) => outer.on_flip(crtc, meta.take()),
                DrmEvent::Error(err) => warn!("DRM error: {err}"),
            })
            .map_err(|err| anyhow::anyhow!("inserting the DRM notifier: {err}"))?;

        let display = open_display(&drm_fd, &mut drm)?;
        let outer = Self {
            handle,
            drm_fd,
            _drm: drm,
            display,
            render_node,
            session: None,
            next_generation: 1,
            on_screen: Shown::Plate,
            in_flight: None,
            queued: None,
        };
        // A restarted outer leaves the screen alone: it still shows the session's last frame
        // (`close_inherited_framebuffers`), and the session's next present replaces it. Lighting
        // the plate here would flash it across every restart.
        if !recovered {
            outer.commit_plate()?;
        }
        Ok(outer)
    }

    /// Light the display with the plate. A modeset, so a commit rather than a flip.
    fn commit_plate(&self) -> anyhow::Result<()> {
        let size = self.mode_size();
        let plane = PlaneState {
            handle: self.display.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((size.0 as f64, size.1 as f64).into()),
                dst: Rectangle::from_size((size.0 as i32, size.1 as i32).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: self.display.plate,
                fence: None,
                hotspot: None,
            }),
        };
        // No page-flip event wanted: nothing waits on the plate, and an event here would land
        // as a flip completion with nothing in flight.
        self.display
            .surface
            .commit([plane], false)
            .context("modesetting the display")?;
        info!(
            "{} lit at {}x{}",
            self.display.connector_name, size.0, size.1
        );
        Ok(())
    }

    fn mode_size(&self) -> (u16, u16) {
        self.display.mode.size()
    }

    fn accept(&mut self, listener: BorrowedFd<'_>) {
        let raw = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        if raw < 0 {
            warn!("accept: {}", io::Error::last_os_error());
            return;
        }
        // SAFETY: accept4 just returned it.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let cred = match peer_cred(fd.as_fd()) {
            Ok(cred) => cred,
            Err(err) => {
                warn!("refusing a connection with no peer credentials: {err}");
                return;
            }
        };
        if let Err(reason) = peer_may_attach(cred.uid) {
            warn!(
                "refusing a session from pid {} uid {}: {reason}",
                cred.pid, cred.uid
            );
            return;
        }
        if self.session.is_some() {
            // One session in this cut. Switching is the next milestone, and it replaces this with
            // fronting the session logind made active (§2.2), never "last one to connect wins".
            warn!(
                "refusing a second session (pid {} uid {}): one is attached",
                cred.pid, cred.uid
            );
            return;
        }
        info!("session connected: pid {} uid {}", cred.pid, cred.uid);

        let fd = Rc::new(fd);
        let generation = self.next_generation;
        self.next_generation += 1;
        let source_fd = fd.clone();
        let token = self
            .handle
            .insert_source(
                Generic::new(FdRef(source_fd), Interest::READ, Mode::Level),
                move |_, _, outer: &mut Outer| {
                    outer.on_session_readable(generation);
                    Ok(PostAction::Continue)
                },
            )
            .expect("inserting a session source");
        self.session = Some(Session {
            fd,
            token,
            generation,
            welcomed: false,
            buffers: HashMap::new(),
        });
    }

    fn on_session_readable(&mut self, generation: u64) {
        loop {
            let Some(session) = self.session.as_ref().filter(|s| s.generation == generation) else {
                return;
            };
            let fd = session.fd.clone();
            match protocol::recv::<ToOuter>(fd.as_fd()) {
                Ok(Received::Message(msg, fds)) => {
                    if fds.len() != msg.fd_count() {
                        warn!(
                            "dropping the session: {msg:?} came with {} fds, expected {}",
                            fds.len(),
                            msg.fd_count()
                        );
                        self.drop_session();
                        return;
                    }
                    if let Err(err) = self.handle_message(msg, fds) {
                        warn!("dropping the session: {err:#}");
                        self.drop_session();
                        return;
                    }
                }
                Ok(Received::WouldBlock) => return,
                Ok(Received::Closed) => {
                    info!("session disconnected");
                    self.drop_session();
                    return;
                }
                Err(err) => {
                    warn!("dropping the session: {err}");
                    self.drop_session();
                    return;
                }
            }
        }
    }

    /// Forget the session. Whatever it last showed stays on screen — its framebuffer is held by
    /// `on_screen` — until something replaces it, so a session that crashes or restarts leaves its
    /// last frame up rather than a flash of plate (§7 is the same rule seen from the other end).
    fn drop_session(&mut self) {
        if let Some(session) = self.session.take() {
            self.handle.remove(session.token);
        }
        if let Some((buffer, _)) = self.queued.take() {
            debug!("dropping queued buffer {buffer} of a departed session");
        }
    }

    fn handle_message(&mut self, msg: ToOuter, mut fds: Vec<OwnedFd>) -> anyhow::Result<()> {
        let session = self.session.as_mut().expect("only called with a session");
        if !session.welcomed && !matches!(msg, ToOuter::Hello { .. }) {
            bail!("expected Hello first, got {msg:?}");
        }

        match msg {
            ToOuter::Hello {
                version,
                session: id,
            } => {
                if session.welcomed {
                    bail!("a second Hello");
                }
                if version != protocol::VERSION {
                    bail!(
                        "protocol version {version}, this outer speaks {}",
                        protocol::VERSION
                    );
                }
                info!("session hello: logind session {id:?}");
                session.welcomed = true;
                let welcome = ToInner::Welcome {
                    version: protocol::VERSION,
                    render_node: self.render_node,
                    outputs: vec![self.display.info.clone()],
                };
                protocol::send(session.fd.as_fd(), &welcome, &[])?;
            }
            ToOuter::AddBuffer {
                id,
                width,
                height,
                fourcc,
                modifier,
                stride,
                offset,
            } => {
                let code = Fourcc::try_from(fourcc)
                    .map_err(|_| anyhow::anyhow!("unknown fourcc {fourcc:#x}"))?;
                let mut builder = Dmabuf::builder(
                    (width as i32, height as i32),
                    code,
                    Modifier::from(modifier),
                    DmabufFlags::empty(),
                );
                builder.add_plane(fds.remove(0), 0, offset, stride);
                let dmabuf = builder.build().context("a dmabuf with no planes")?;
                let fb = framebuffer_from_dmabuf(&self.drm_fd, &dmabuf, true)
                    .map_err(|err| anyhow::anyhow!("importing buffer {id}: {err}"))?;
                debug!("session buffer {id}: {width}x{height} {code:?} {modifier:#x}");
                session.buffers.insert(id, Rc::new(fb));
            }
            ToOuter::RemoveBuffer { id } => {
                session.buffers.remove(&id);
            }
            ToOuter::Present {
                output,
                buffer,
                fenced,
            } => {
                if output != self.display.connector_name {
                    bail!("present on unknown output {output}");
                }
                if !session.buffers.contains_key(&buffer) {
                    bail!("present of unknown buffer {buffer}");
                }
                let fence = fenced.then(|| fds.remove(0));
                if self.in_flight.is_some() {
                    if let Some((stale, _)) = self.queued.replace((buffer, fence)) {
                        self.discard(stale);
                    }
                } else {
                    self.flip(buffer, fence);
                }
            }
        }
        Ok(())
    }

    /// Flip `buffer` onto the screen, gated on `fence` in the kernel (`IN_FENCE_FD`), so the
    /// session's render is never shown half-done and the outer never blocks on it.
    fn flip(&mut self, buffer: u32, fence: Option<OwnedFd>) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(fb) = session.buffers.get(&buffer).cloned() else {
            return;
        };
        let generation = session.generation;
        let size = self.mode_size();
        let plane = PlaneState {
            handle: self.display.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((size.0 as f64, size.1 as f64).into()),
                dst: Rectangle::from_size((size.0 as i32, size.1 as i32).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: *(*fb).as_ref(),
                fence: fence.as_ref().map(|fd| fd.as_fd()),
                hotspot: None,
            }),
        };
        match self.display.surface.page_flip([plane], true) {
            Ok(()) => {
                self.in_flight = Some(Shown::Session {
                    buffer,
                    _fb: fb,
                    generation,
                });
            }
            Err(err) => {
                warn!("flip of buffer {buffer} refused: {err}");
                self.discard(buffer);
            }
        }
    }

    fn discard(&self, buffer: u32) {
        if let Some(session) = &self.session {
            let msg = ToInner::Discarded {
                output: self.display.connector_name.clone(),
                buffer,
            };
            if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
                warn!("telling the session buffer {buffer} was discarded: {err}");
            }
        }
    }

    fn on_flip(&mut self, _crtc: crtc::Handle, meta: Option<DrmEventMetadata>) {
        let Some(shown) = self.in_flight.take() else {
            debug!("flip completion with nothing in flight");
            return;
        };

        if let Shown::Session {
            buffer, generation, ..
        } = &shown
        {
            if let Some(session) = self
                .session
                .as_ref()
                .filter(|s| s.generation == *generation)
            {
                let (sequence, time_ns) = match meta {
                    Some(DrmEventMetadata {
                        sequence,
                        time: DrmEventTime::Monotonic(time),
                    }) => (sequence, time.as_nanos() as u64),
                    Some(DrmEventMetadata { sequence, .. }) => (sequence, 0),
                    None => (0, 0),
                };
                let msg = ToInner::Presented {
                    output: self.display.connector_name.clone(),
                    buffer: *buffer,
                    sequence,
                    time_ns,
                    refresh_ns: self.display.refresh.as_nanos() as u64,
                };
                if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
                    warn!("reporting a flip to the session: {err}");
                }
            }
        }
        // Dropping the previous frame here, not earlier: it was on the screen until this moment.
        self.on_screen = shown;

        if let Some((buffer, fence)) = self.queued.take() {
            self.flip(buffer, fence);
        }
    }
}

/// An `Rc<OwnedFd>` the event loop can poll while the session keeps its own reference for sends.
struct FdRef(Rc<OwnedFd>);

impl AsFd for FdRef {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Pick the first connected connector, its preferred mode and a CRTC that can drive it, and make
/// the plate for it.
fn open_display(drm_fd: &DrmDeviceFd, drm: &mut DrmDevice) -> anyhow::Result<Display> {
    let res = drm_fd.resource_handles().context("reading DRM resources")?;
    let conn = res
        .connectors()
        .iter()
        .filter_map(|&c| drm_fd.get_connector(c, true).ok())
        .find(|c| c.state() == connector::State::Connected && !c.modes().is_empty())
        .context("no connected display")?;
    let connector_name = format_connector_name(&conn);

    let modes = conn.modes();
    let mode = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(&modes[0])
        .to_owned();

    let crtc = conn
        .encoders()
        .iter()
        .filter_map(|&e| drm_fd.get_encoder(e).ok())
        .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
        .next()
        .context("no CRTC can drive the display")?;
    let surface = drm
        .create_surface(crtc, mode, &[conn.handle()])
        .context("creating the DRM surface")?;

    let (w, h) = mode.size();
    let mut plate_buffer = drm_fd
        .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)
        .context("allocating the plate")?;
    {
        let mut map = drm_fd
            .map_dumb_buffer(&mut plate_buffer)
            .context("mapping the plate")?;
        // GNOME's dark plate (`$system_bg_color`, #242424 in libadwaita dark), the colour a
        // session's own background sits on. Opaque and flat: this is a herald surface, not chrome.
        for px in map.as_mut().as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[0x24, 0x24, 0x24, 0xff]);
        }
    }
    let plate = drm_fd
        .add_framebuffer(&plate_buffer, 24, 32)
        .context("adding the plate framebuffer")?;

    let refresh = refresh_interval(mode);
    let to_info = |m: &smithay::reexports::drm::control::Mode| ModeInfo {
        width: m.size().0,
        height: m.size().1,
        refresh_mhz: (1_000_000_000_000u64 / refresh_interval(*m).as_nanos().max(1) as u64) as u32,
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
    };
    let info = OutputInfo {
        connector: connector_name.clone(),
        edid: edid_blob(drm, conn.handle())
            .map_err(|err| debug!("no EDID for {connector_name}: {err:#}"))
            .ok(),
        physical_size_mm: conn.size().unwrap_or((0, 0)),
        mode: to_info(&mode),
        modes: modes.iter().map(to_info).collect(),
    };

    Ok(Display {
        connector_name,
        surface,
        mode,
        info,
        refresh,
        _plate_buffer: plate_buffer,
        plate,
    })
}

struct PeerCred {
    pid: i32,
    uid: u32,
}

fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<PeerCred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    let rv = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rv < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred {
        pid: cred.pid,
        uid: cred.uid,
    })
}

/// Whether a process running as `uid` may attach as the fronted session.
///
/// The rule is logind's, not ours: the session that may be on screen is the one logind made
/// active on the seat, which is also the one whose input it routes (§2.2). Read from logind's seat
/// state file, the same source `sd_seat_get_active` reads.
fn peer_may_attach(uid: u32) -> Result<(), String> {
    let state = std::fs::read_to_string("/run/systemd/seats/seat0")
        .map_err(|err| format!("reading seat0's state: {err}"))?;
    let active_uid = state
        .lines()
        .find_map(|line| line.strip_prefix("ACTIVE_UID="))
        .ok_or("seat0 has no active session")?;
    if active_uid.trim() == uid.to_string() {
        Ok(())
    } else {
        Err(format!(
            "seat0's active session belongs to uid {}",
            active_uid.trim()
        ))
    }
}
