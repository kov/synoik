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
//! a plate until a session is fronted, then the fronted session unredirected, with its render
//! fence handed to the commit as `IN_FENCE_FD` and the kernel's flip timestamps passed back. Any
//! number of sessions may be attached; the one on screen is always the one logind made active on
//! seat0 (§2.2), and nothing a background session presents reaches the screen. The DRM fd lives
//! in systemd's fd store, so a restarted outer is still master and the session's last frame stays
//! on screen across the gap (§1, §7).

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

    let seat_watch = watch_seats()?;
    handle
        .insert_source(
            Generic::new(seat_watch, Interest::READ, Mode::Level),
            |_, watch, outer: &mut Outer| {
                drain_inotify(watch.as_fd());
                outer.refresh_front();
                Ok(PostAction::Continue)
            },
        )
        .map_err(|err| anyhow::anyhow!("inserting the seat watch: {err}"))?;
    outer.refresh_front();

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

/// What to flip once the flip in flight completes.
enum Next {
    Plate,
    Session {
        generation: u64,
        buffer: u32,
        fence: Option<Rc<OwnedFd>>,
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

/// An attached session.
struct Session {
    fd: Rc<OwnedFd>,
    token: RegistrationToken,
    /// Per connection, so a flip that completes after a session left is not reported to the next
    /// one.
    generation: u64,
    uid: u32,
    pid: i32,
    /// The logind session it proved to be, once `Hello` is accepted.
    logind_id: Option<String>,
    buffers: HashMap<u32, Rc<PrimeFramebuffer>>,
    /// Its newest frame and the fence that completes it — what goes on screen the moment it is
    /// fronted again, without waiting for it to draw.
    latest: Option<(u32, Option<Rc<OwnedFd>>)>,
}

struct Outer {
    handle: LoopHandle<'static, Outer>,
    drm_fd: DrmDeviceFd,
    _drm: DrmDevice,
    display: Display,
    render_node: Option<(u64, u64)>,
    sessions: HashMap<u64, Session>,
    next_generation: u64,
    /// seat0's active logind session, as logind last wrote it.
    front: Option<String>,
    /// The last frame whose flip completed.
    on_screen: Shown,
    /// The flip in flight, if any. One at a time: the kernel refuses a second flip on a CRTC with
    /// one pending.
    in_flight: Option<Shown>,
    /// What to show once the flip in flight lands; latest wins.
    queued: Option<Next>,
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
            sessions: HashMap::new(),
            next_generation: 1,
            front: None,
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
        debug!("connection from pid {} uid {}", cred.pid, cred.uid);

        let fd = Rc::new(fd);
        let generation = self.next_generation;
        self.next_generation += 1;
        let token = self
            .handle
            .insert_source(
                Generic::new(FdRef(fd.clone()), Interest::READ, Mode::Level),
                move |_, _, outer: &mut Outer| {
                    outer.on_session_readable(generation);
                    Ok(PostAction::Continue)
                },
            )
            .expect("inserting a session source");
        self.sessions.insert(
            generation,
            Session {
                fd,
                token,
                generation,
                uid: cred.uid,
                pid: cred.pid,
                logind_id: None,
                buffers: HashMap::new(),
                latest: None,
            },
        );
    }

    fn on_session_readable(&mut self, generation: u64) {
        loop {
            let Some(session) = self.sessions.get(&generation) else {
                return;
            };
            let fd = session.fd.clone();
            match protocol::recv::<ToOuter>(fd.as_fd()) {
                Ok(Received::Message(msg, fds)) => {
                    if fds.len() != msg.fd_count() {
                        warn!(
                            "dropping a session: {msg:?} came with {} fds, expected {}",
                            fds.len(),
                            msg.fd_count()
                        );
                        self.drop_session(generation);
                        return;
                    }
                    if let Err(err) = self.handle_message(generation, msg, fds) {
                        warn!("dropping a session: {err:#}");
                        self.drop_session(generation);
                        return;
                    }
                }
                Ok(Received::WouldBlock) => return,
                Ok(Received::Closed) => {
                    info!("session {:?} disconnected", session.logind_id);
                    self.drop_session(generation);
                    return;
                }
                Err(err) => {
                    warn!("dropping a session: {err}");
                    self.drop_session(generation);
                    return;
                }
            }
        }
    }

    /// Forget a session. If it is on screen its frame stays up — the framebuffer is held by
    /// `on_screen` — until something replaces it, so a fronted session that crashes or restarts
    /// leaves its last frame rather than a flash of plate (§7 seen from the other end).
    fn drop_session(&mut self, generation: u64) {
        if let Some(session) = self.sessions.remove(&generation) {
            self.handle.remove(session.token);
        }
        if matches!(self.queued, Some(Next::Session { generation: g, .. }) if g == generation) {
            self.queued = None;
        }
    }

    /// The attached session logind has fronted, if any.
    fn fronted(&self) -> Option<u64> {
        let front = self.front.as_deref()?;
        self.sessions
            .values()
            .find(|s| s.logind_id.as_deref() == Some(front))
            .map(|s| s.generation)
    }

    /// Re-read seat0's active session and, if it changed, put the new one on screen.
    ///
    /// The screen follows logind and nothing else (§2.2): logind is what moved input to that
    /// session, so the session on screen and the session typed into can never disagree.
    fn refresh_front(&mut self) {
        let front = read_seat_active("seat0");
        if front == self.front {
            return;
        }
        info!("seat0 fronts session {front:?} (was {:?})", self.front);
        self.front = front;

        // Never leave the previous session's frame up for whoever is at the seat now: show the
        // new session's newest frame, or the plate until it draws one.
        let next = match self.fronted().and_then(|g| self.sessions.get(&g)) {
            Some(Session {
                generation,
                latest: Some((buffer, fence)),
                ..
            }) => Next::Session {
                generation: *generation,
                buffer: *buffer,
                fence: fence.clone(),
            },
            _ => Next::Plate,
        };
        self.show(next);
    }

    /// Show `next` now, or once the flip in flight lands.
    fn show(&mut self, next: Next) {
        if self.in_flight.is_some() {
            if let Some(Next::Session {
                generation, buffer, ..
            }) = self.queued.replace(next)
            {
                self.discard(generation, buffer);
            }
        } else {
            self.flip(next);
        }
    }

    fn handle_message(
        &mut self,
        generation: u64,
        msg: ToOuter,
        mut fds: Vec<OwnedFd>,
    ) -> anyhow::Result<()> {
        let session = self
            .sessions
            .get_mut(&generation)
            .expect("only called for an attached session");
        if session.logind_id.is_none() && !matches!(msg, ToOuter::Hello { .. }) {
            bail!("expected Hello first, got {msg:?}");
        }

        match msg {
            ToOuter::Hello {
                version,
                session: id,
            } => {
                if session.logind_id.is_some() {
                    bail!("a second Hello");
                }
                if version != protocol::VERSION {
                    bail!(
                        "protocol version {version}, this outer speaks {}",
                        protocol::VERSION
                    );
                }
                let id = id.context("Hello names no logind session")?;
                verify_session_claim(&id, session.uid, session.pid)
                    .with_context(|| format!("pid {} claiming session {id}", session.pid))?;
                info!(
                    "session {id} attached (pid {} uid {})",
                    session.pid, session.uid
                );
                session.logind_id = Some(id);
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
                if matches!(session.latest, Some((b, _)) if b == id) {
                    session.latest = None;
                }
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
                let fence = fenced.then(|| Rc::new(fds.remove(0)));
                session.latest = Some((buffer, fence.clone()));
                if self.fronted() == Some(generation) {
                    self.show(Next::Session {
                        generation,
                        buffer,
                        fence,
                    });
                } else {
                    // A background session does not reach the screen. It hears so at once, so it
                    // is not left waiting for a flip, and it keeps the frame as its latest.
                    self.discard(generation, buffer);
                }
            }
        }
        Ok(())
    }

    /// Put `next` on the screen. A session frame is gated on its fence in the kernel
    /// (`IN_FENCE_FD`), so it is never shown half-rendered and the outer never blocks on it.
    fn flip(&mut self, next: Next) {
        let size = self.mode_size();
        let (fb_handle, shown, fence) = match next {
            Next::Plate => (self.display.plate, Shown::Plate, None),
            Next::Session {
                generation,
                buffer,
                fence,
            } => {
                let Some(fb) = self
                    .sessions
                    .get(&generation)
                    .and_then(|s| s.buffers.get(&buffer))
                    .cloned()
                else {
                    return;
                };
                let handle = *(*fb).as_ref();
                let shown = Shown::Session {
                    buffer,
                    _fb: fb,
                    generation,
                };
                (handle, shown, fence)
            }
        };
        let plane = PlaneState {
            handle: self.display.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((size.0 as f64, size.1 as f64).into()),
                dst: Rectangle::from_size((size.0 as i32, size.1 as i32).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: fb_handle,
                fence: fence.as_ref().map(|fd| fd.as_fd()),
                hotspot: None,
            }),
        };
        match self.display.surface.page_flip([plane], true) {
            Ok(()) => self.in_flight = Some(shown),
            Err(err) => {
                warn!("flip refused: {err}");
                if let Shown::Session {
                    generation, buffer, ..
                } = shown
                {
                    self.discard(generation, buffer);
                }
            }
        }
    }

    fn discard(&self, generation: u64, buffer: u32) {
        if let Some(session) = self.sessions.get(&generation) {
            let msg = ToInner::Discarded {
                output: self.display.connector_name.clone(),
                buffer,
            };
            if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
                warn!("telling a session buffer {buffer} was discarded: {err}");
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
            if let Some(session) = self.sessions.get(generation) {
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
                    warn!("reporting a flip to a session: {err}");
                }
            }
        }
        // Dropping the previous frame here, not earlier: it was on the screen until this moment.
        self.on_screen = shown;

        if let Some(next) = self.queued.take() {
            self.flip(next);
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

/// Check that a peer is entitled to the logind session it names.
///
/// A claim is accepted when the session exists on seat0, belongs to the peer's uid, and — when
/// the peer runs inside a session scope at all — is that scope's session. That last case is the
/// common one (a session compositor started from the session's login process); one started as a
/// user service runs outside any scope, and then owning the session is what entitles it, which
/// is also all logind itself asks before handing that uid the session's devices.
fn verify_session_claim(id: &str, uid: u32, pid: i32) -> anyhow::Result<()> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("not a session id: {id:?}");
    }
    let state = std::fs::read_to_string(format!("/run/systemd/sessions/{id}"))
        .with_context(|| format!("no logind session {id}"))?;
    let field = |key: &str| {
        state
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::to_owned)
    };
    if field("UID").as_deref() != Some(uid.to_string().as_str()) {
        bail!("session {id} does not belong to uid {uid}");
    }
    if field("SEAT").as_deref() != Some("seat0") {
        bail!("session {id} is not on seat0");
    }
    if let Some(scope) = session_of_pid(pid) {
        if scope != id {
            bail!("the peer runs in session {scope}");
        }
    }
    Ok(())
}

/// The logind session whose scope `pid` runs in, read the way `sd_pid_get_session` does.
fn session_of_pid(pid: i32) -> Option<String> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup
        .split('/')
        .find_map(|part| part.strip_prefix("session-")?.strip_suffix(".scope"))
        .map(|id| id.trim().to_owned())
}

/// The active session of `seat`, from logind's own state file — what `sd_seat_get_active` reads.
fn read_seat_active(seat: &str) -> Option<String> {
    let state = std::fs::read_to_string(format!("/run/systemd/seats/{seat}")).ok()?;
    state
        .lines()
        .find_map(|line| line.strip_prefix("ACTIVE="))
        .map(|id| id.trim().to_owned())
        .filter(|id| !id.is_empty())
}

/// An inotify watch on logind's seat state, the mechanism `sd_login_monitor` uses: logind
/// rewrites `/run/systemd/seats/<seat>` by rename on every change of the active session.
fn watch_seats() -> anyhow::Result<OwnedFd> {
    let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if raw < 0 {
        return Err(io::Error::last_os_error()).context("inotify_init1");
    }
    // SAFETY: just created.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let path = c"/run/systemd/seats";
    let mask = libc::IN_MOVED_TO | libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_DELETE;
    if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
        return Err(io::Error::last_os_error()).context("watching /run/systemd/seats");
    }
    Ok(fd)
}

fn drain_inotify(fd: BorrowedFd<'_>) {
    let mut buf = [0u8; 4096];
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}
