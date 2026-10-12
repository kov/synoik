// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The system compositor: holds DRM master for the machine and scans a nested session out
//! (`docs/fork/system-compositor.md`).
//!
//! Deliberately not a `Synoik`. In steady state it composites nothing — it flips the fronted
//! session's own buffers — so it is a small event loop over the DRM device and one socket, and
//! that thinness is what keeps restart and reattach cheap (§1, §7). It draws only to animate a
//! session switch (§5, `compose`).
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
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::drm::buffer::DrmFourcc;
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::{connector, crtc, framebuffer, Device as _, ModeTypeFlags};
use smithay::reexports::rustix::fs as rfs;
use smithay::utils::{DeviceFd, Rectangle, Transform};

use super::chord;
use super::compose::{Compositor, Picture};
use super::protocol::{self, ModeInfo, OutputInfo, Received, ToInner, ToOuter};
use crate::backend::tty::{edid_blob, format_connector_name, refresh_interval};
use crate::backend::vulkan_scanout::{framebuffer_from_dmabuf, PrimeFramebuffer};
use crate::render_helpers::vulkan::VkTexture;
use crate::utils::get_monotonic_time;

/// The name the DRM fd is stored under in systemd's fd store.
const FDSTORE_NAME: &str = "drm";

/// GNOME's dark plate (`$system_bg_color`, #242424 in libadwaita dark), the colour a session's
/// own background sits on.
const PLATE: [u8; 3] = [0x24, 0x24, 0x24];

/// How long a switch crossfades (§5 steps 2 and 7, which with no modeset are one fade).
const FADE: Duration = Duration::from_millis(250);

/// How long the outgoing session's picture may stay up waiting for the incoming one's first
/// frame. Past it the switch fades to the incoming session's last frame, or the plate: the
/// previous user's screen is not left in front of whoever is at the seat now.
const TARGET_WAIT: Duration = Duration::from_millis(300);

/// How long the greeter's picture may stay up instead (greeter.md §4.4). A user session's first
/// frame takes seconds after a login, and the greeter shows nothing private, so it holds until the
/// incoming frame rather than fading the login to the plate; bounded, so a session that never
/// draws still gets the plate.
const GREETER_TARGET_WAIT: Duration = Duration::from_secs(10);

/// How long a switch away from a session of logind class `outgoing` waits for the incoming frame.
fn target_wait(outgoing: Option<&str>) -> Duration {
    match outgoing {
        Some("greeter") => GREETER_TARGET_WAIT,
        _ => TARGET_WAIT,
    }
}

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

    if chord::wanted() {
        if let Err(err) = chord::watch(&handle, Outer::on_chord) {
            warn!("no switch chord: {err:#}");
        }
    }
    if let Err(err) = watch_shutdown(&handle) {
        warn!("no fade at shutdown: {err:#}");
    }

    notify(&[sd_notify::NotifyState::Ready]);
    event_loop.run(None, &mut outer, |_| ())?;
    Ok(())
}

/// What the screen fades to as the machine goes down.
const BLACK: [f32; 3] = [0., 0., 0.];

/// Deliver logind's `PrepareForShutdown` to the loop. A thread of its own: the outer's loop is
/// calloop, not an executor, and the signal is all it wants from the bus.
fn watch_shutdown(handle: &LoopHandle<'static, Outer>) -> anyhow::Result<()> {
    let (tx, rx) = smithay::reexports::calloop::channel::channel::<bool>();
    handle
        .insert_source(rx, |event, _, outer: &mut Outer| {
            if let smithay::reexports::calloop::channel::Event::Msg(going) = event {
                outer.on_prepare_for_shutdown(going);
            }
        })
        .map_err(|err| anyhow::anyhow!("inserting the shutdown channel: {err}"))?;
    std::thread::Builder::new()
        .name("logind-shutdown".into())
        .spawn(move || {
            let watch = || -> zbus::Result<()> {
                let bus = zbus::blocking::Connection::system()?;
                let manager = zbus::blocking::Proxy::new(
                    &bus,
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                )?;
                for signal in manager.receive_signal("PrepareForShutdown")? {
                    let going: bool = signal.body().deserialize()?;
                    if tx.send(going).is_err() {
                        break;
                    }
                }
                Ok(())
            };
            if let Err(err) = watch() {
                warn!("watching for shutdown: {err}");
            }
        })
        .context("spawning the shutdown watch")?;
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
        /// Its contents, for the snapshot a switch takes of it.
        dmabuf: Dmabuf,
        generation: u64,
        /// Whether its flip answers a `Present`. A frame shown again (`last_presented`) was
        /// answered already, and a second `Presented` would be taken for whatever the session
        /// has in flight now.
        answer: bool,
    },
    /// A frame the outer drew itself, in its own scanout slot: the switch's `from` and `to`
    /// pictures blended at `alpha`.
    Composite {
        slot: usize,
        _fb: Rc<PrimeFramebuffer>,
        alpha: f32,
        /// The incoming session frame it blends in, while that frame's present is unanswered:
        /// the flip putting the blend on screen answers it.
        samples: Option<(u64, u32)>,
    },
}

/// What to flip once the flip in flight completes.
enum Next {
    Plate,
    Session {
        generation: u64,
        buffer: u32,
        fence: Option<Rc<OwnedFd>>,
        answer: bool,
    },
    Composite {
        slot: usize,
        fb: Rc<PrimeFramebuffer>,
        fence: Option<Rc<OwnedFd>>,
        alpha: f32,
        samples: Option<(u64, u32)>,
    },
}

/// A session switch being animated (§5): `from` crossfading to `to`, which is unknown until the
/// incoming session's frame is ready. `to` is live: each frame the incoming session presents during
/// the fade replaces it, so whatever that session animates (an unlock, its startup) plays through
/// the fade instead of freezing on its first frame and cutting at the end.
struct Transition {
    from: Picture,
    to: Option<Target>,
    /// Waiting for this session frame's render fence before it can become `to`.
    pending: Option<(u64, u32, Option<Rc<OwnedFd>>)>,
    pending_watch: Option<RegistrationToken>,
    /// When the fade began, on the monotonic clock.
    start: Duration,
    /// Armed while there is neither `to` nor `pending` (`TARGET_WAIT`).
    wait: Option<RegistrationToken>,
    /// The outgoing greeter, still drawing: until the incoming session has a frame, the greeter's
    /// frames go on screen as they come, so its dialog fades out in front of the user — logind
    /// activates a new session on a VT-less seat the moment it is created, before the greeter has
    /// had the chance (`seat_attach_session`). `from` is re-taken when the target arrives.
    from_live: Option<u64>,
}

struct Target {
    picture: Picture,
    /// What goes on screen, unredirected, when the fade completes.
    end: End,
}

#[derive(Clone, Copy, PartialEq)]
enum End {
    /// Nothing replaces the fade's last frame: the shutdown's black stays on screen.
    Hold,
    Plate,
    Session {
        generation: u64,
        buffer: u32,
        answer: bool,
    },
}

/// A buffer a session registered.
struct SessionBuffer {
    fb: Rc<PrimeFramebuffer>,
    dmabuf: Dmabuf,
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
    buffers: HashMap<u32, SessionBuffer>,
    /// The newest frame it presented while in the background, left unanswered: the inner keeps
    /// that slot reserved for as long as the present is outstanding, so it can be flipped the
    /// moment the session is fronted without the inner rendering into it underneath the scanout.
    held: Option<(u32, Option<Rc<OwnedFd>>)>,
    /// The last frame reported `Presented` to it. The inner never draws into the slot it was last
    /// told is on screen, so this too is safe to show again on a switch back.
    last_presented: Option<u32>,
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
    /// The outer's own drawing, for switch transitions. `None` without a usable GPU, and then a
    /// switch is a cut.
    compositor: Option<Compositor>,
    transition: Option<Transition>,
    /// A switch waiting for the flip in flight to land before it snapshots the screen.
    switch_pending: bool,
    /// How long the switch in progress waits for the incoming frame: the outgoing session's
    /// class decides (`target_wait`).
    switch_wait: Duration,
    /// What the last finished transition faded to, standing in for its frame on screen.
    last_to: Option<Picture>,
    /// The greeter the switch in progress leaves, for [`Transition::from_live`].
    outgoing_greeter: Option<u64>,
    /// logind announced the machine is going down (`PrepareForShutdown(true)`): the screen fades
    /// to black and stays there, whatever logind fronts as the sessions end.
    shutting_down: bool,
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
        let compositor = render_node.and_then(|node| {
            Compositor::new(node, display.mode.size())
                .map_err(|err| warn!("no transitions, session switches will cut: {err:#}"))
                .ok()
        });
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
            compositor,
            transition: None,
            switch_pending: false,
            switch_wait: TARGET_WAIT,
            last_to: None,
            outgoing_greeter: None,
            shutting_down: false,
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
                held: None,
                last_presented: None,
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
        // A switch waiting on this session's frame would wait forever.
        let pending = self.transition.as_mut().and_then(|t| {
            t.pending
                .take_if(|(g, _, _)| *g == generation)
                .and(t.pending_watch.take())
        });
        if let Some(token) = pending {
            self.handle.remove(token);
            self.set_target(Target {
                picture: Picture::Color(PLATE.map(|c| f32::from(c) / 255.)),
                end: End::Plate,
            });
        }
    }

    /// `PrepareForShutdown`: fade to black and hold it while the machine goes down (§5), or, when
    /// another inhibitor calls the shutdown off, bring back whatever logind fronts.
    fn on_prepare_for_shutdown(&mut self, going: bool) {
        if going == self.shutting_down {
            return;
        }
        self.shutting_down = going;
        if !going {
            info!("the shutdown was called off");
            // Re-read from scratch so the session logind fronts comes back as a switch.
            self.front = None;
            self.refresh_front();
            return;
        }
        info!("the machine is going down: fading to black");
        if self.compositor.is_none() {
            return;
        }
        if self.in_flight.is_some() {
            self.switch_pending = true;
        } else {
            self.begin_switch();
        }
    }

    /// Ctrl+Alt+F<n>: front the n-th session on seat0, in login order, by asking logind to
    /// activate it (§2.2) — the screen then follows the active session as for any switch.
    fn on_chord(&mut self, n: u32) {
        let sessions = seat_sessions("seat0");
        let Some(id) = sessions.get(n as usize - 1) else {
            debug!("Ctrl+Alt+F{n}: seat0 has {} sessions", sessions.len());
            return;
        };
        if self.front.as_ref() == Some(id) {
            return;
        }
        info!("Ctrl+Alt+F{n}: activating session {id}");
        activate_session(id.clone());
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
        if self.shutting_down {
            // The sessions are ending under the fade to black; none of them comes back on.
            self.front = front;
            return;
        }
        info!("seat0 fronts session {front:?} (was {:?})", self.front);
        let outgoing = self.front.as_deref().and_then(read_session_class);
        self.switch_wait = target_wait(outgoing.as_deref());
        self.outgoing_greeter = (outgoing.as_deref() == Some("greeter"))
            .then(|| self.fronted())
            .flatten();
        self.front = front;

        if self.compositor.is_some() {
            if self.in_flight.is_some() {
                // The snapshot must be of what is on screen, and that changes when this flip
                // lands; start from there.
                self.switch_pending = true;
            } else {
                self.begin_switch();
            }
            return;
        }

        // A cut. Never leave the previous session's frame up for whoever is at the seat now: show
        // the new session's newest frame, or the plate until it draws one. Only frames the inner
        // cannot be drawing into qualify (`held`, `last_presented`); a frame it was told was
        // discarded is a free slot to it, and scanning that out would race its next render.
        let next = match self.fronted().and_then(|g| self.sessions.get_mut(&g)) {
            Some(session) => match (session.held.take(), session.last_presented) {
                (Some((buffer, fence)), _) => Next::Session {
                    generation: session.generation,
                    buffer,
                    fence,
                    answer: true,
                },
                (None, Some(buffer)) => Next::Session {
                    generation: session.generation,
                    buffer,
                    fence: None,
                    answer: false,
                },
                (None, None) => Next::Plate,
            },
            None => Next::Plate,
        };
        self.show(next);
    }

    /// Start animating to the newly fronted session (§5).
    ///
    /// Step 1 is the snapshot: the outgoing picture is copied out of the session's buffer, so the
    /// fade does not depend on that session drawing, answering or even living. logind already
    /// moved input when it changed the active session, so nothing typed during the fade reaches
    /// the outgoing session.
    fn begin_switch(&mut self) {
        let from = self.snapshot_screen();
        self.cancel_transition();
        // A frame of the outgoing session waiting to flip is that session's newest; it waits for
        // the switch back instead, still reserved.
        if let Some(next) = self.queued.take() {
            match next {
                Next::Session {
                    generation,
                    buffer,
                    fence,
                    answer: true,
                } => self.hold(generation, buffer, fence),
                Next::Session { answer: false, .. } => (),
                Next::Plate | Next::Composite { .. } => (),
            }
        }
        self.transition = Some(Transition {
            from,
            to: None,
            pending: None,
            pending_watch: None,
            start: get_monotonic_time(),
            wait: None,
            from_live: self.outgoing_greeter.take(),
        });

        if self.shutting_down {
            self.set_target(Target {
                picture: Picture::Color(BLACK),
                end: End::Hold,
            });
            return;
        }

        let held = self
            .fronted()
            .and_then(|g| Some((g, self.sessions.get_mut(&g)?.held.take()?)));
        match held {
            // Its newest frame is already here: the session drew it, then was switched away
            // before it could be shown, and it draws nothing more until that frame is answered.
            Some((generation, (buffer, fence))) => self.target_frame(generation, buffer, fence),
            None => self.arm_target_wait(),
        }
    }

    /// The picture on screen now, as something a fade can start from.
    fn snapshot_screen(&mut self) -> Picture {
        let plate = Picture::Color(PLATE.map(|c| f32::from(c) / 255.));
        let compositor = self.compositor.as_mut().expect("only with a compositor");
        let captured = match &self.on_screen {
            Shown::Plate => return plate,
            Shown::Session { dmabuf, .. } => compositor.snapshot(dmabuf),
            // Mid-fade: the blend on screen, redrawn, so the next fade starts where this one
            // stands instead of jumping to either end.
            Shown::Composite { alpha, .. } => match &self.transition {
                Some(Transition {
                    from, to: Some(to), ..
                }) => compositor.capture(from, &to.picture, *alpha),
                Some(Transition { from, .. }) => return from.clone(),
                None => return self.last_to.clone().unwrap_or(plate),
            },
        };
        match captured {
            Ok(texture) => Picture::Texture(texture),
            Err(err) => {
                warn!("snapshotting the screen: {err:#}");
                plate
            }
        }
    }

    /// Drop the transition, returning a session frame it was holding to that session's `held`.
    fn cancel_transition(&mut self) {
        let Some(transition) = self.transition.take() else {
            return;
        };
        if let Some(token) = transition.wait {
            self.handle.remove(token);
        }
        if let Some(token) = transition.pending_watch {
            self.handle.remove(token);
        }
        if let Some((generation, buffer, fence)) = transition.pending {
            self.hold(generation, buffer, fence);
        }
        if let Some(Target {
            end:
                End::Session {
                    generation,
                    buffer,
                    answer: true,
                },
            ..
        }) = transition.to
        {
            // Unanswered still; the session is waiting on it.
            self.hold(generation, buffer, None);
        }
    }

    /// Keep `buffer` as `generation`'s newest unanswered frame, giving back the one it replaces.
    fn hold(&mut self, generation: u64, buffer: u32, fence: Option<Rc<OwnedFd>>) {
        let Some(session) = self.sessions.get_mut(&generation) else {
            return;
        };
        if let Some((stale, _)) = session.held.replace((buffer, fence)) {
            if stale != buffer {
                self.discard(generation, stale);
            }
        }
    }

    fn arm_target_wait(&mut self) {
        // Not the compositor clock of `Synoik::timer_after`: the outer holds no `Synoik`.
        #[allow(clippy::disallowed_methods)]
        let timer = Timer::from_duration(self.switch_wait);
        let token = self
            .handle
            .insert_source(timer, |_, _, outer: &mut Outer| {
                outer.on_target_wait();
                TimeoutAction::Drop
            })
            .expect("inserting a timer");
        if let Some(transition) = &mut self.transition {
            transition.wait = Some(token);
        }
    }

    /// The incoming session has not drawn in time: fade to its last frame, or the plate.
    fn on_target_wait(&mut self) {
        let Some(transition) = &mut self.transition else {
            return;
        };
        transition.wait = None;
        if transition.to.is_some() || transition.pending.is_some() {
            return;
        }
        let last = self
            .fronted()
            .and_then(|g| Some((g, self.sessions.get(&g)?.last_presented?)));
        debug!("switch target late; fading to {last:?}");
        let target = last
            .and_then(|(generation, buffer)| {
                let texture = self.import_frame(generation, buffer)?;
                Some(Target {
                    picture: Picture::Texture(texture),
                    end: End::Session {
                        generation,
                        buffer,
                        answer: false,
                    },
                })
            })
            .unwrap_or(Target {
                picture: Picture::Color(PLATE.map(|c| f32::from(c) / 255.)),
                end: End::Plate,
            });
        self.set_target(target);
    }

    /// A frame of the incoming session becomes the target once its render is complete. Its
    /// present stays unanswered until it is flipped as it is at the end (§3.1: a target is not
    /// told it is on screen before it is).
    fn target_frame(&mut self, generation: u64, buffer: u32, fence: Option<Rc<OwnedFd>>) {
        let signalled = fence.as_ref().is_none_or(|fd| fence_signalled(fd.as_fd()));
        let Some(transition) = &mut self.transition else {
            return;
        };
        if let Some(token) = transition.wait.take() {
            self.handle.remove(token);
        }
        // A newer frame supersedes one still rendering: give that one back.
        if let Some(token) = transition.pending_watch.take() {
            self.handle.remove(token);
        }
        if let Some((g, stale, _)) = transition.pending.take() {
            self.discard(g, stale);
        }
        if signalled {
            self.target_ready(generation, buffer);
            return;
        }
        let fd = fence.clone().expect("unsignalled means fenced");
        let token = self
            .handle
            .insert_source(
                Generic::new(FdRef(fd), Interest::READ, Mode::OneShot),
                move |_, _, outer: &mut Outer| {
                    if let Some(transition) = &mut outer.transition {
                        if matches!(transition.pending, Some((g, b, _)) if g == generation && b == buffer)
                        {
                            transition.pending = None;
                            transition.pending_watch = None;
                            outer.target_ready(generation, buffer);
                        }
                    }
                    Ok(PostAction::Remove)
                },
            )
            .expect("inserting a fence watch");
        let transition = self.transition.as_mut().expect("checked above");
        transition.pending = Some((generation, buffer, fence));
        transition.pending_watch = Some(token);
    }

    fn target_ready(&mut self, generation: u64, buffer: u32) {
        match self.import_frame(generation, buffer) {
            Some(texture) => self.set_target(Target {
                picture: Picture::Texture(texture),
                end: End::Session {
                    generation,
                    buffer,
                    answer: true,
                },
            }),
            None => {
                // Unusable as a picture; show it without the fade rather than not at all.
                self.cancel_transition();
                self.show(Next::Session {
                    generation,
                    buffer,
                    fence: None,
                    answer: true,
                });
            }
        }
    }

    fn import_frame(&mut self, generation: u64, buffer: u32) -> Option<VkTexture> {
        let dmabuf = &self.sessions.get(&generation)?.buffers.get(&buffer)?.dmabuf;
        let compositor = self.compositor.as_mut()?;
        compositor
            .import(dmabuf)
            .map_err(|err| warn!("{err:#}"))
            .ok()
    }

    /// Make `target` what the fade goes to. The first target starts the fade's clock; a newer
    /// frame replacing it (the live `to`) keeps it, and gives back the frame it replaces if no
    /// blend has shown that one yet.
    fn set_target(&mut self, target: Target) {
        // The outgoing greeter stops being live: the fade starts from its newest frame on screen.
        if let Some(live) = self.transition.as_mut().and_then(|t| t.from_live.take()) {
            if let Some(Next::Session {
                generation,
                buffer,
                answer: true,
                ..
            }) = self
                .queued
                .take_if(|n| matches!(n, Next::Session { generation, .. } if *generation == live))
            {
                self.discard(generation, buffer);
            }
            let from = self.snapshot_screen();
            if let Some(transition) = &mut self.transition {
                transition.from = from;
            }
        }
        let Some(transition) = &mut self.transition else {
            return;
        };
        match transition.to.replace(target) {
            None => transition.start = get_monotonic_time(),
            Some(Target {
                end:
                    End::Session {
                        generation,
                        buffer,
                        answer: true,
                    },
                ..
            }) => {
                // A blend in flight may still be reading it: the inner must not draw into it
                // until that blend lands, which gives it back then (`on_flip`).
                let sampled = matches!(
                    self.in_flight,
                    Some(Shown::Composite { samples: Some(s), .. }) if s == (generation, buffer)
                );
                if !sampled {
                    self.discard(generation, buffer);
                }
            }
            Some(_) => (),
        }
        if self.in_flight.is_none() {
            self.advance();
        }
    }

    fn progress(&self, start: Duration) -> f32 {
        let elapsed = get_monotonic_time().saturating_sub(start);
        (elapsed.as_secs_f32() / FADE.as_secs_f32()).clamp(0., 1.)
    }

    /// Draw and flip the next frame of the fade, or finish it. Runs once per flip completion, so
    /// the fade is paced by the display.
    fn advance(&mut self) {
        let Some(transition) = &self.transition else {
            return;
        };
        let Some(to) = &transition.to else {
            return;
        };
        let t = self.progress(transition.start);
        let settled = matches!(self.on_screen, Shown::Composite { alpha, .. } if alpha >= 1.);
        if t >= 1. && (to.end != End::Hold || settled) {
            self.finish_transition();
            return;
        }
        // Ease out: most of the change early, settling into the incoming picture. A fade nothing
        // replaces draws its end too: its last frame is what stays on screen.
        let alpha = 1. - (1. - t).powi(3);
        let alpha = if t >= 1. { 1. } else { alpha };
        let busy = match &self.on_screen {
            Shown::Composite { slot, .. } => Some(*slot),
            _ => None,
        };
        let compositor = self
            .compositor
            .as_mut()
            .expect("transitions need a compositor");
        let samples = match to.end {
            End::Session {
                generation,
                buffer,
                answer: true,
            } => Some((generation, buffer)),
            _ => None,
        };
        match compositor.compose(&self.drm_fd, busy, &transition.from, &to.picture, alpha) {
            Ok((slot, fb, fence)) => self.flip(Next::Composite {
                slot,
                fb,
                fence: fence.map(Rc::new),
                alpha,
                samples,
            }),
            Err(err) => {
                warn!("drawing a transition frame: {err:#}");
                self.finish_transition();
            }
        }
    }

    /// Step 8: the fade is over, so put the incoming session's frame on screen as it is.
    fn finish_transition(&mut self) {
        let Some(transition) = self.transition.take() else {
            return;
        };
        if let Some(token) = transition.wait {
            self.handle.remove(token);
        }
        // A newer frame still rendering when the fade ends — routine with a live target — is the
        // session's newest: held, so the flip after the fade's end shows it. Dropped, its present
        // would go unanswered and the session would never draw again.
        if let Some(token) = transition.pending_watch {
            self.handle.remove(token);
        }
        if let Some((generation, buffer, fence)) = transition.pending {
            debug!("a fade ended with frame {buffer} still rendering");
            self.hold(generation, buffer, fence);
        }
        let Some(to) = transition.to else {
            return;
        };
        self.last_to = Some(to.picture);
        if to.end == End::Hold {
            return;
        }
        let end = match to.end {
            End::Session {
                generation,
                buffer,
                answer,
            } if self
                .sessions
                .get(&generation)
                .is_some_and(|s| s.buffers.contains_key(&buffer)) =>
            {
                Next::Session {
                    generation,
                    buffer,
                    fence: None,
                    answer,
                }
            }
            _ => Next::Plate,
        };
        let ended_on_plate = matches!(end, Next::Plate);
        self.show(end);

        // The incoming session drew while the fade was running on a stand-in.
        let held = self
            .fronted()
            .and_then(|g| Some((g, self.sessions.get_mut(&g)?.held.take()?)));
        if let Some((generation, (buffer, fence))) = held {
            self.fronted_frame(generation, buffer, fence, ended_on_plate);
        }
    }

    /// A frame of the fronted session, outside a switch. It replaces the plate with a fade, and
    /// anything else as it is.
    fn fronted_frame(
        &mut self,
        generation: u64,
        buffer: u32,
        fence: Option<Rc<OwnedFd>>,
        over_plate: bool,
    ) {
        if over_plate && self.compositor.is_some() {
            self.transition = Some(Transition {
                from: Picture::Color(PLATE.map(|c| f32::from(c) / 255.)),
                to: None,
                pending: None,
                pending_watch: None,
                start: get_monotonic_time(),
                wait: None,
                from_live: None,
            });
            self.target_frame(generation, buffer, fence);
        } else {
            self.show(Next::Session {
                generation,
                buffer,
                fence,
                answer: true,
            });
        }
    }

    /// Show `next` now, or once the flip in flight lands.
    fn show(&mut self, next: Next) {
        if self.in_flight.is_some() {
            if let Some(Next::Session {
                generation,
                buffer,
                answer: true,
                ..
            }) = self.queued.replace(next)
            {
                self.discard(generation, buffer);
            }
        } else {
            self.flip(next);
        }
    }

    /// Whether the plate is what the screen shows, or is about to.
    fn plate_showing(&self) -> bool {
        match (&self.in_flight, &self.queued) {
            (_, Some(next)) => matches!(next, Next::Plate),
            (Some(shown), None) => matches!(shown, Shown::Plate),
            (None, None) => matches!(self.on_screen, Shown::Plate),
        }
    }

    fn handle_message(
        &mut self,
        generation: u64,
        msg: ToOuter,
        mut fds: Vec<OwnedFd>,
    ) -> anyhow::Result<()> {
        // Going down, no session reaches the screen again: their frames are held, unanswered.
        let fronted = !self.shutting_down && self.fronted() == Some(generation);
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
                session.buffers.insert(
                    id,
                    SessionBuffer {
                        fb: Rc::new(fb),
                        dmabuf,
                    },
                );
            }
            ToOuter::RemoveBuffer { id } => {
                session.buffers.remove(&id);
                if matches!(session.held, Some((b, _)) if b == id) {
                    session.held = None;
                }
                if session.last_presented == Some(id) {
                    session.last_presented = None;
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
                let awaiting = self
                    .transition
                    .as_ref()
                    .map(|t| t.to.is_none() && t.pending.is_none());
                let feeds_fade = self.transition.as_ref().is_some_and(|t| {
                    t.pending.as_ref().is_some_and(|(g, ..)| *g == generation)
                        || matches!(
                            t.to,
                            Some(Target { end: End::Session { generation: g, .. }, .. })
                                if g == generation
                        )
                });
                match (fronted, awaiting) {
                    // The frame the switch to this session was waiting for (§5 step 6), or a newer
                    // one replacing it while the fade runs.
                    (true, Some(true)) => self.target_frame(generation, buffer, fence),
                    (true, Some(false)) if feeds_fade => {
                        self.target_frame(generation, buffer, fence)
                    }
                    (false, Some(true))
                        if self
                            .transition
                            .as_ref()
                            .is_some_and(|t| t.from_live == Some(generation)) =>
                    {
                        self.show(Next::Session {
                            generation,
                            buffer,
                            fence,
                            answer: true,
                        })
                    }
                    (true, None) => {
                        let over_plate = self.plate_showing();
                        self.fronted_frame(generation, buffer, fence, over_plate);
                    }
                    // A background session does not reach the screen, nor does a fronted one
                    // while the switch to it runs on another frame. Its newest frame is held
                    // unanswered; the one it supersedes is given back.
                    _ => self.hold(generation, buffer, fence),
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
                answer,
            } => {
                let Some(SessionBuffer { fb, dmabuf }) = self
                    .sessions
                    .get(&generation)
                    .and_then(|s| s.buffers.get(&buffer))
                else {
                    return;
                };
                let handle = *(**fb).as_ref();
                let shown = Shown::Session {
                    buffer,
                    _fb: fb.clone(),
                    dmabuf: dmabuf.clone(),
                    generation,
                    answer,
                };
                (handle, shown, fence)
            }
            Next::Composite {
                slot,
                fb,
                fence,
                alpha,
                samples,
            } => {
                let handle = *(*fb).as_ref();
                (
                    handle,
                    Shown::Composite {
                        slot,
                        _fb: fb,
                        alpha,
                        samples,
                    },
                    fence,
                )
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
                match shown {
                    Shown::Session {
                        generation,
                        buffer,
                        answer: true,
                        ..
                    } => self.discard(generation, buffer),
                    Shown::Session { answer: false, .. } => (),
                    // No flip completion will come to draw the next frame: end the fade now.
                    Shown::Composite { .. } => self.finish_transition(),
                    Shown::Plate => (),
                }
            }
        }
    }

    /// Whether `buffer` is still spoken for — held, queued, awaited by a fade, or what the session
    /// was last told is on screen — so it must not be given back.
    fn reserved(&self, generation: u64, buffer: u32) -> bool {
        let session = self.sessions.get(&generation);
        session.is_some_and(|s| {
            s.held.as_ref().is_some_and(|(b, _)| *b == buffer) || s.last_presented == Some(buffer)
        }) || matches!(
            self.queued,
            Some(Next::Session { generation: g, buffer: b, .. }) if g == generation && b == buffer
        ) || self.transition.as_ref().is_some_and(|t| {
            matches!(t.pending, Some((g, b, _)) if g == generation && b == buffer)
                || matches!(
                    t.to,
                    Some(Target { end: End::Session { generation: g, buffer: b, .. }, .. })
                        if g == generation && b == buffer
                )
        })
    }

    /// Tell `generation` its `buffer` reached the screen.
    fn presented(&mut self, generation: u64, buffer: u32, meta: Option<&DrmEventMetadata>) {
        let Some(session) = self.sessions.get_mut(&generation) else {
            return;
        };
        session.last_presented = Some(buffer);
        let (sequence, time_ns) = match meta {
            Some(DrmEventMetadata {
                sequence,
                time: DrmEventTime::Monotonic(time),
            }) => (*sequence, time.as_nanos() as u64),
            Some(DrmEventMetadata { sequence, .. }) => (*sequence, 0),
            None => (0, 0),
        };
        let msg = ToInner::Presented {
            output: self.display.connector_name.clone(),
            buffer,
            sequence,
            time_ns,
            refresh_ns: self.display.refresh.as_nanos() as u64,
        };
        if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
            warn!("reporting a flip to a session: {err}");
        }
    }

    fn discard(&self, generation: u64, buffer: u32) {
        if let Some(session) = self.sessions.get(&generation) {
            debug!("discarding buffer {buffer} of {:?}", session.logind_id);
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

        match &shown {
            Shown::Session {
                buffer,
                generation,
                answer: true,
                ..
            } => self.presented(*generation, *buffer, meta.as_ref()),
            // A blend of the incoming session's frame is that frame on screen, as far as the
            // session's pacing goes: answer it, so it draws its next one.
            Shown::Composite {
                samples: Some((generation, buffer)),
                ..
            } => {
                let (generation, buffer) = (*generation, *buffer);
                // `Some(unanswered)` while it is still the target.
                let target = match self.transition.as_mut().and_then(|t| t.to.as_mut()) {
                    Some(Target {
                        end:
                            End::Session {
                                generation: g,
                                buffer: b,
                                answer,
                            },
                        ..
                    }) if *g == generation && *b == buffer => Some(std::mem::take(answer)),
                    _ => None,
                };
                match target {
                    Some(true) => self.presented(generation, buffer, meta.as_ref()),
                    Some(false) => (),
                    // Replaced while this blend was reading it (`set_target`): free now — unless
                    // it went back to the session meanwhile (a cancelled fade holds its target).
                    None if !self.reserved(generation, buffer) => self.discard(generation, buffer),
                    None => (),
                }
            }
            _ => (),
        }
        // Dropping the previous frame here, not earlier: it was on the screen until this moment.
        self.on_screen = shown;

        if self.switch_pending {
            // Now that the screen has settled, take the switch from what it shows.
            self.switch_pending = false;
            self.begin_switch();
        } else if let Some(next) = self.queued.take() {
            self.flip(next);
        } else if self.transition.is_some() {
            self.advance();
        }
    }
}

/// Whether a sync_file has signalled: it polls readable once it has.
fn fence_signalled(fd: BorrowedFd<'_>) -> bool {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, 0) > 0 }
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
        // Opaque and flat: this is a herald surface, not chrome.
        let [r, g, b] = PLATE;
        for px in map.as_mut().as_chunks_mut::<4>().0 {
            px.copy_from_slice(&[b, g, r, 0xff]);
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

/// A logind session's class (`user`, `greeter`, …), from its state file.
fn read_session_class(id: &str) -> Option<String> {
    let state = std::fs::read_to_string(format!("/run/systemd/sessions/{id}")).ok()?;
    state
        .lines()
        .find_map(|line| line.strip_prefix("CLASS="))
        .map(|class| class.trim().to_owned())
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

/// The sessions on `seat` that can be fronted: the greeter, then users, oldest login first. The
/// greeter leads even when it was reopened after the users logged in, so Ctrl+Alt+F1 is always
/// the login screen, as VT1 is under GDM (greeter.md §5).
fn seat_sessions(seat: &str) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/run/systemd/sessions") else {
        return Vec::new();
    };
    let sessions: Vec<(String, u64, String)> = dir
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let id = entry.file_name().into_string().ok()?;
            if id.contains('.') {
                // `<id>.ref` and the like: not session state.
                return None;
            }
            let state = std::fs::read_to_string(entry.path()).ok()?;
            let field = |key: &str| {
                state
                    .lines()
                    .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            };
            let fronts = field("SEAT") == Some(seat)
                && matches!(field("CLASS"), Some("user" | "greeter"))
                && matches!(field("STATE"), Some("online" | "active"));
            let since = field("REALTIME")?.parse().ok()?;
            fronts.then_some((field("CLASS")?.to_owned(), since, id))
        })
        .collect();
    front_order(sessions)
}

/// `(class, login time, id)` in chord order: greeter-class first, then by login time.
fn front_order(mut sessions: Vec<(String, u64, String)>) -> Vec<String> {
    sessions.sort_by_key(|(class, since, _)| (class != "greeter", *since));
    sessions.into_iter().map(|(_, _, id)| id).collect()
}

/// Ask logind to make session `id` the active one on its seat. Off the event loop: the call goes
/// through polkit (`org.freedesktop.login1.chvt`, §1), and the outer must keep flipping while it
/// is answered.
fn activate_session(id: String) {
    let spawned = std::thread::Builder::new()
        .name("activate-session".into())
        .spawn(move || {
            let result = zbus::blocking::Connection::system().and_then(|bus| {
                bus.call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "ActivateSession",
                    &(id.as_str(),),
                )
            });
            if let Err(err) = result {
                warn!("activating session {id}: {err}");
            }
        });
    if let Err(err) = spawned {
        warn!("spawning the session activation: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A greeter reopened after the users logged in still answers Ctrl+Alt+F1.
    #[test]
    fn the_greeter_leads_the_chord_order() {
        let order = front_order(vec![
            ("user".into(), 20, "2".into()),
            ("greeter".into(), 30, "c3".into()),
            ("user".into(), 10, "5".into()),
        ]);
        assert_eq!(order, ["c3", "5", "2"]);
    }

    /// Leaving the greeter holds its picture for the incoming session's first frame; leaving a user
    /// does not (greeter.md §4.4).
    #[test]
    fn only_the_greeter_holds_for_the_incoming_frame() {
        assert_eq!(target_wait(Some("greeter")), GREETER_TARGET_WAIT);
        assert_eq!(target_wait(Some("user")), TARGET_WAIT);
        assert_eq!(target_wait(None), TARGET_WAIT);
    }
}
