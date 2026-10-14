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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use anyhow::{bail, Context as _};
use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmEvent, DrmEventMetadata, DrmEventTime, DrmNode, DrmSurface,
    NodeType, PlaneConfig, PlaneState,
};
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::drm::buffer::DrmFourcc;
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::{
    connector, crtc, framebuffer, Device as _, Mode as DrmMode, ModeFlags, ModeTypeFlags,
};
use smithay::reexports::rustix::fs as rfs;
use smithay::utils::{DeviceFd, Rectangle, Transform};
use smithay_drm_extras::drm_scanner::DrmScanner;

mod boot;

use self::boot::{BootSplash, Plymouth};
use super::chord;
use super::compose::{Canvas, Compositor, Picture};
use super::protocol::{self, ConnectorInfo, ModeInfo, OutputConfig, Received, ToInner, ToOuter};
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
    // The first modeset put plymouth's frame back up; the spinner turns from there, and the socket
    // is plymouthd's once it lets go of it (boot-splash.md §2.1).
    outer.begin_spinner();
    outer.bind_plymouth();
    if let Err(err) = watch_hotplug(&handle, &outer) {
        warn!("displays plugged in later will not light: {err:#}");
    }

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

/// Follow the card's connectors: udev reports a change on the card for every hotplug, mode-list
/// or EDID change, which is all a resized VM window is.
fn watch_hotplug(handle: &LoopHandle<'static, Outer>, outer: &Outer) -> anyhow::Result<()> {
    let card = rfs::fstat(outer.drm_fd.as_fd())
        .context("stat of the DRM fd")?
        .st_rdev;
    let udev = UdevBackend::new("seat0").context("opening udev")?;
    handle
        .insert_source(udev, move |event, _, outer: &mut Outer| {
            if let UdevEvent::Changed { device_id } = event {
                if device_id == card {
                    outer.on_hotplug();
                }
            }
        })
        .map_err(|err| anyhow::anyhow!("inserting the udev source: {err}"))?;
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

/// What is on (or on its way to) a display.
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
    /// The boot splash in place of the plate, drawn in one of the outer's own scanout slots: its
    /// `revision` and spinner `frame`, to tell when it is out of date.
    Splash {
        slot: usize,
        _fb: Rc<PrimeFramebuffer>,
        revision: u64,
        frame: usize,
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

/// A session switch being animated on one display (§5): `from` crossfading to `to`, which is
/// unknown until the incoming session's frame is ready. `to` is live: each frame the incoming
/// session presents during the fade replaces it, so whatever that session animates (an unlock, its
/// startup) plays through the fade instead of freezing on its first frame and cutting at the end.
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

/// A connected connector the outer can light: it has a CRTC to drive it.
struct Connector {
    handle: connector::Handle,
    crtc: crtc::Handle,
    info: ConnectorInfo,
    /// `info.modes`' DRM modes, index for index.
    modes: Vec<DrmMode>,
}

impl Connector {
    fn preferred(&self) -> DrmMode {
        self.modes
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(&self.modes[0])
            .to_owned()
    }

    /// The advertised mode a session asked for.
    fn find(&self, mode: &ModeInfo) -> Option<DrmMode> {
        self.info
            .modes
            .iter()
            .position(|m| {
                (m.width, m.height, m.refresh_mhz) == (mode.width, mode.height, mode.refresh_mhz)
            })
            .map(|i| self.modes[i])
    }
}

/// A lit display: one connector on its CRTC, and everything on its way to it.
struct Display {
    crtc: crtc::Handle,
    surface: DrmSurface,
    mode: DrmMode,
    refresh: Duration,
    /// Kept alive for as long as the plate may be shown.
    _plate_buffer: DumbBuffer,
    plate: framebuffer::Handle,
    /// The outer's own scanout slots here, for transitions.
    canvas: Canvas,
    /// The last frame whose flip completed.
    on_screen: Shown,
    /// The flip in flight, if any. One at a time: the kernel refuses a second flip on a CRTC with
    /// one pending.
    in_flight: Option<Shown>,
    /// What to show once the flip in flight lands; latest wins.
    queued: Option<Next>,
    transition: Option<Transition>,
    /// A switch waiting for the flip in flight to land before it snapshots the screen.
    switch_pending: bool,
    /// The layout changes this display (its mode, or off), and waits for the flip in flight to
    /// land first: a modeset on a CRTC with a flip pending would have the flip's completion
    /// answer for whatever is flipped after it.
    relayout_pending: bool,
    /// What the last finished transition faded to, standing in for its frame on screen.
    last_to: Option<Picture>,
}

impl Display {
    fn size(&self) -> (u16, u16) {
        self.mode.size()
    }
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
    /// The displays it lights, and whether they are on at all, as its last `Configure` asked;
    /// `None` until it sends one.
    config: Option<(Vec<OutputConfig>, bool)>,
    /// Per display, the newest frame it presented while that frame could not be shown — the
    /// session is in the background, or the display is not lit for it — left unanswered: the
    /// inner keeps that slot reserved for as long as the present is outstanding, so it can be
    /// flipped the moment the session is fronted without the inner rendering into it underneath
    /// the scanout.
    held: HashMap<String, (u32, Option<Rc<OwnedFd>>)>,
    /// Per display, the last frame reported `Presented` to it. The inner never draws into the slot
    /// it was last told is on screen, so this too is safe to show again on a switch back.
    last_presented: HashMap<String, u32>,
}

struct Outer {
    handle: LoopHandle<'static, Outer>,
    drm_fd: DrmDeviceFd,
    drm: DrmDevice,
    scanner: DrmScanner,
    /// Every connected connector with a CRTC, by name.
    connectors: BTreeMap<String, Connector>,
    /// The lit ones, by connector name.
    displays: BTreeMap<String, Display>,
    /// A restarted outer adopts the CRTCs its predecessor lit instead of lighting them again,
    /// until the first layout is applied.
    recovering: bool,
    render_node: Option<(u64, u64)>,
    sessions: HashMap<u64, Session>,
    next_generation: u64,
    /// seat0's active logind session, as logind last wrote it.
    front: Option<String>,
    /// The outer's own drawing, for switch transitions. `None` without a usable GPU, and then a
    /// switch is a cut.
    compositor: Option<Compositor>,
    /// How long the switch in progress waits for the incoming frame: the outgoing session's
    /// class decides (`target_wait`).
    switch_wait: Duration,
    /// The greeter the switch in progress leaves, for [`Transition::from_live`].
    outgoing_greeter: Option<u64>,
    /// logind announced the machine is going down (`PrepareForShutdown(true)`): the screen fades
    /// to black and stays there, whatever logind fronts as the sessions end.
    shutting_down: bool,
    /// plymouthd's socket, answered by the outer.
    plymouth: Plymouth,
    /// The boot splash, while it stands in for the plate: from a fresh start until a session's
    /// screen replaces it.
    boot: Option<BootSplash>,
}

fn plate_picture() -> Picture {
    Picture::Color(PLATE.map(|c| f32::from(c) / 255.))
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
        let (drm, notifier) =
            DrmDevice::new(drm_fd.clone(), false).context("creating the DRM device")?;
        handle
            .insert_source(notifier, |event, meta, outer: &mut Outer| match event {
                DrmEvent::VBlank(crtc) => outer.on_flip(crtc, meta.take()),
                DrmEvent::Error(err) => warn!("DRM error: {err}"),
            })
            .map_err(|err| anyhow::anyhow!("inserting the DRM notifier: {err}"))?;

        let compositor = render_node.and_then(|node| {
            Compositor::new(node)
                .map_err(|err| warn!("no transitions, session switches will cut: {err:#}"))
                .ok()
        });
        let mut outer = Self {
            handle,
            drm_fd,
            drm,
            scanner: DrmScanner::new(),
            connectors: BTreeMap::new(),
            displays: BTreeMap::new(),
            recovering: recovered,
            render_node,
            sessions: HashMap::new(),
            next_generation: 1,
            front: None,
            compositor,
            switch_wait: TARGET_WAIT,
            outgoing_greeter: None,
            shutting_down: false,
            plymouth: Plymouth::new(),
            boot: None,
        };
        if !recovered && outer.compositor.is_some() {
            outer.start_boot_splash();
        }
        outer.scan_connectors();
        if outer.connectors.is_empty() {
            bail!("no connected display");
        }
        outer.relayout_and_fill();
        outer.recovering = false;
        Ok(outer)
    }

    /// Re-read the connectors. Returns whether anything a session sees changed.
    fn scan_connectors(&mut self) -> bool {
        if let Err(err) = self.scanner.scan_connectors(&self.drm_fd) {
            warn!("scanning connectors: {err}");
            return false;
        }
        let mut connectors = BTreeMap::new();
        for (conn, crtc) in self.scanner.crtcs() {
            if conn.state() != connector::State::Connected {
                continue;
            }
            let modes: Vec<DrmMode> = conn
                .modes()
                .iter()
                // Interlaced modes don't work (the tty's `pick_mode` skips them too).
                .filter(|m| !m.flags().contains(ModeFlags::INTERLACE))
                .copied()
                .collect();
            if modes.is_empty() {
                continue;
            }
            let name = format_connector_name(conn);
            let info = ConnectorInfo {
                connector: name.clone(),
                edid: edid_blob(&self.drm, conn.handle())
                    .map_err(|err| debug!("no EDID for {name}: {err:#}"))
                    .ok(),
                physical_size_mm: conn.size().unwrap_or((0, 0)),
                modes: modes.iter().map(mode_info).collect(),
            };
            connectors.insert(
                name,
                Connector {
                    handle: conn.handle(),
                    crtc,
                    info,
                    modes,
                },
            );
        }
        let changed = connectors.len() != self.connectors.len()
            || connectors
                .iter()
                .zip(&self.connectors)
                .any(|((a, ca), (b, cb))| a != b || ca.info != cb.info);
        self.connectors = connectors;
        changed
    }

    fn connector_infos(&self) -> Vec<ConnectorInfo> {
        self.connectors.values().map(|c| c.info.clone()).collect()
    }

    /// A display was plugged, unplugged or changed: tell every session, and follow the fronted
    /// one's configuration onto what is there now.
    fn on_hotplug(&mut self) {
        if !self.scan_connectors() {
            return;
        }
        info!(
            "displays now: {:?}",
            self.connectors.keys().collect::<Vec<_>>()
        );
        let msg = ToInner::Connectors {
            connectors: self.connector_infos(),
        };
        for session in self.sessions.values().filter(|s| s.logind_id.is_some()) {
            if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
                warn!("telling a session the displays changed: {err}");
            }
        }
        self.relayout_and_fill();
    }

    /// The displays to light and their modes: what the fronted session configured. A configured
    /// mode the connector stopped advertising keeps the mode it runs, rather than going dark until
    /// the session catches up: the session answers the `Connectors` that told it with a
    /// `Configure` of its own.
    ///
    /// Until the fronted session has configured — it is still starting, or not attached — the
    /// layout stays as it is, so a login's fade does not modeset under it on the way: the new
    /// session's `Configure` lands before its first frame, while the greeter is still held up.
    /// A connector nothing lit yet comes on at its preferred mode (§4.1).
    fn wanted_layout(&self) -> Vec<(String, DrmMode)> {
        if self.shutting_down {
            return self
                .displays
                .iter()
                .map(|(name, d)| (name.clone(), d.mode))
                .collect();
        }
        let config = self
            .fronted()
            .and_then(|g| self.sessions.get(&g)?.config.as_ref());
        match config {
            Some((_, false)) => Vec::new(),
            Some((outputs, true)) => outputs
                .iter()
                .filter_map(|o| {
                    let conn = self.connectors.get(&o.connector)?;
                    let mode = conn
                        .find(&o.mode)
                        .or_else(|| Some(self.displays.get(&o.connector)?.mode))
                        .unwrap_or_else(|| conn.preferred());
                    Some((o.connector.clone(), mode))
                })
                .collect(),
            None => self
                .connectors
                .iter()
                .map(|(name, c)| {
                    let mode = self
                        .displays
                        .get(name)
                        .map_or_else(|| c.preferred(), |d| d.mode);
                    (name.clone(), mode)
                })
                .collect(),
        }
    }

    /// Bring the displays to [`Self::wanted_layout`]. Returns the displays it left as they were;
    /// every other lit one shows the plate, or waits for its flip to land first.
    fn relayout(&mut self) -> HashSet<String> {
        let wanted = self.wanted_layout();
        let names: Vec<String> = self.displays.keys().cloned().collect();
        for name in names {
            if !wanted.iter().any(|(n, _)| *n == name) {
                self.unlight(&name);
            }
        }
        let mut kept = HashSet::new();
        for (name, mode) in wanted {
            match self.displays.get(&name) {
                Some(d) if d.mode == mode && !d.relayout_pending => {
                    kept.insert(name);
                }
                Some(_) => self.remode(&name, mode),
                None => self.light(&name, mode),
            }
        }
        kept
    }

    /// [`Self::relayout`], then put the fronted session's newest frame on every display that was
    /// changed under it.
    fn relayout_and_fill(&mut self) {
        let kept = self.relayout();
        let changed: Vec<String> = self
            .displays
            .iter()
            .filter(|(name, d)| !kept.contains(*name) && !d.relayout_pending)
            .map(|(name, _)| name.clone())
            .collect();
        for name in changed {
            self.show_held(&name, true);
        }
    }

    /// The fronted session's held frame for `d`, if it was drawn for the display as it is now.
    /// One drawn for another mode is given back: it can never be shown.
    fn take_held(&mut self, d: &str) -> Option<(u64, u32, Option<Rc<OwnedFd>>)> {
        let generation = self.fronted()?;
        let (buffer, fence) = self.sessions.get_mut(&generation)?.held.remove(d)?;
        if self.fits(generation, buffer, d) {
            Some((generation, buffer, fence))
        } else {
            self.discard(generation, d, buffer);
            None
        }
    }

    /// Show the fronted session's held frame on `d`, if it has one.
    fn show_held(&mut self, d: &str, over_plate: bool) {
        if let Some((generation, buffer, fence)) = self.take_held(d) {
            self.fronted_frame(d, generation, buffer, fence, over_plate);
        }
    }

    /// Whether `buffer` is the size of display `d`'s mode.
    fn fits(&self, generation: u64, buffer: u32, d: &str) -> bool {
        let Some(display) = self.displays.get(d) else {
            return false;
        };
        let (w, h) = display.size();
        self.sessions
            .get(&generation)
            .and_then(|s| s.buffers.get(&buffer))
            .is_some_and(|b| {
                let size = b.dmabuf.size();
                (size.w, size.h) == (i32::from(w), i32::from(h))
            })
    }

    /// Stop showing frames on `d`: a modeset or a turn-off follows. Everything owed an answer gets
    /// one, or goes back to its session.
    fn retire(&mut self, d: &str) {
        self.cancel_transition(d);
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        display.switch_pending = false;
        display.last_to = None;
        if let Some(Next::Session {
            generation,
            buffer,
            answer: true,
            ..
        }) = display.queued.take()
        {
            self.discard(generation, d, buffer);
        }
    }

    fn light(&mut self, d: &str, mode: DrmMode) {
        let Some(conn) = self.connectors.get(d) else {
            return;
        };
        let (crtc, handle) = (conn.crtc, conn.handle);
        // A restarted outer finds the display still lit by its predecessor, the session's last
        // frame on it (§7): adopt it as it is, at whatever mode the session had it.
        let running = self
            .recovering
            .then(|| self.drm_fd.get_crtc(crtc).ok()?.mode())
            .flatten();
        let adopt = running.is_some();
        let mode = running.unwrap_or(mode);
        let surface = match self.drm.create_surface(crtc, mode, &[handle]) {
            Ok(surface) => surface,
            Err(err) => {
                warn!("lighting {d}: {err}");
                return;
            }
        };
        let (plate_buffer, plate) = match make_plate(&self.drm_fd, mode.size()) {
            Ok(plate) => plate,
            Err(err) => {
                warn!("lighting {d}: {err:#}");
                return;
            }
        };
        let display = Display {
            crtc,
            surface,
            mode,
            refresh: refresh_interval(mode),
            _plate_buffer: plate_buffer,
            plate,
            canvas: Canvas::new(mode.size()),
            on_screen: Shown::Plate,
            in_flight: None,
            queued: None,
            transition: None,
            switch_pending: false,
            relayout_pending: false,
            last_to: None,
        };
        self.displays.insert(d.to_owned(), display);
        if adopt {
            info!("{d} adopted at {}x{}", mode.size().0, mode.size().1);
        } else if let Err(err) = self.commit_base(d) {
            warn!("{err:#}");
            self.displays.remove(d);
        }
    }

    /// The frame that stands for no session on `d`: the boot splash while it is up, else the
    /// plate. `None` when the splash cannot be drawn.
    fn draw_base(&mut self, d: &str) -> Option<(framebuffer::Handle, Shown, Option<OwnedFd>)> {
        if self.boot.is_none() {
            let display = self.displays.get(d)?;
            return Some((display.plate, Shown::Plate, None));
        }
        let (revision, frame) = self.splash_state();
        let splash = self.plymouth.server.splash().clone();
        let display = self.displays.get_mut(d)?;
        let busy = match &display.on_screen {
            Shown::Composite { slot, .. } | Shown::Splash { slot, .. } => Some(*slot),
            _ => None,
        };
        let compositor = self.compositor.as_mut()?;
        match compositor.draw_splash(&mut display.canvas, &self.drm_fd, busy, &splash, 1., frame) {
            Ok((slot, fb, fence)) => Some((
                *(*fb).as_ref(),
                Shown::Splash {
                    slot,
                    _fb: fb,
                    revision,
                    frame,
                },
                fence,
            )),
            Err(err) => {
                warn!("drawing the splash on {d}: {err:#}");
                let display = self.displays.get(d)?;
                Some((display.plate, Shown::Plate, None))
            }
        }
    }

    /// Modeset `d` showing [`Self::draw_base`].
    fn commit_base(&mut self, d: &str) -> anyhow::Result<()> {
        let (fb, shown, fence) = self.draw_base(d).context("no display")?;
        if let Some(fence) = fence {
            // Once per modeset: the commit carries no fence of its own.
            wait_fence(fence.as_fd());
        }
        let display = self.displays.get_mut(d).context("no display")?;
        commit_frame(d, display, fb)?;
        display.on_screen = shown;
        Ok(())
    }

    fn unlight(&mut self, d: &str) {
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        if display.in_flight.is_some() {
            display.relayout_pending = true;
            return;
        }
        self.retire(d);
        // Dropping the surface turns the CRTC off.
        self.displays.remove(d);
        info!("{d} off");
    }

    fn remode(&mut self, d: &str, mode: DrmMode) {
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        if display.in_flight.is_some() {
            display.relayout_pending = true;
            return;
        }
        self.retire(d);
        let (plate_buffer, plate) = match make_plate(&self.drm_fd, mode.size()) {
            Ok(plate) => plate,
            Err(err) => {
                warn!("changing the mode of {d}: {err:#}");
                return;
            }
        };
        let display = self.displays.get_mut(d).expect("checked above");
        if let Err(err) = display.surface.use_mode(mode) {
            warn!("changing the mode of {d}: {err}");
            return;
        }
        display.mode = mode;
        display.refresh = refresh_interval(mode);
        display._plate_buffer = plate_buffer;
        display.plate = plate;
        display.canvas = Canvas::new(mode.size());
        display.on_screen = Shown::Plate;
        if let Err(err) = self.commit_base(d) {
            warn!("{err:#}");
            self.displays.remove(d);
        }
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
                config: None,
                held: HashMap::new(),
                last_presented: HashMap::new(),
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
        let names: Vec<String> = self.displays.keys().cloned().collect();
        for d in names {
            let display = self.displays.get_mut(&d).expect("listed above");
            if matches!(display.queued, Some(Next::Session { generation: g, .. }) if g == generation)
            {
                display.queued = None;
            }
            // A switch waiting on this session's frame would wait forever.
            let pending = display.transition.as_mut().and_then(|t| {
                t.pending
                    .take_if(|(g, _, _)| *g == generation)
                    .and(t.pending_watch.take())
            });
            if let Some(token) = pending {
                self.handle.remove(token);
                let picture = self.base_picture(&d);
                self.set_target(
                    &d,
                    Target {
                        picture,
                        end: End::Plate,
                    },
                );
            }
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
        self.outgoing_greeter = None;
        let names: Vec<String> = self.displays.keys().cloned().collect();
        for d in names {
            self.switch_display(&d);
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

        // The incoming session's display configuration first (§3: the fronted session's wins). A
        // display it changes the mode of, lights or turns off is a cut — through the plate, under
        // which the modeset is hidden (§4.2); every other one crossfades.
        let kept = self.relayout();
        let names: Vec<String> = self.displays.keys().cloned().collect();
        for d in names {
            if kept.contains(&d) {
                self.switch_display(&d);
            } else if !self.displays[&d].relayout_pending {
                self.show_held(&d, true);
            }
        }
    }

    /// Switch display `d` to the fronted session: a crossfade with a compositor, else a cut.
    fn switch_display(&mut self, d: &str) {
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        if self.compositor.is_some() {
            if display.in_flight.is_some() {
                // The snapshot must be of what is on screen, and that changes when this flip
                // lands; start from there.
                display.switch_pending = true;
            } else {
                self.begin_switch(d);
            }
            return;
        }

        // A cut. Never leave the previous session's frame up for whoever is at the seat now: show
        // the new session's newest frame, or the plate until it draws one. Only frames the inner
        // cannot be drawing into qualify (`held`, `last_presented`); a frame it was told was
        // discarded is a free slot to it, and scanning that out would race its next render.
        let next = match self.take_held(d) {
            Some((generation, buffer, fence)) => Next::Session {
                generation,
                buffer,
                fence,
                answer: true,
            },
            None => match self.last_presented_on(d) {
                Some((generation, buffer)) => Next::Session {
                    generation,
                    buffer,
                    fence: None,
                    answer: false,
                },
                None => Next::Plate,
            },
        };
        self.show(d, next);
    }

    /// The fronted session's last frame answered on `d`, if it still fits the display.
    fn last_presented_on(&self, d: &str) -> Option<(u64, u32)> {
        let generation = self.fronted()?;
        let buffer = *self.sessions.get(&generation)?.last_presented.get(d)?;
        self.fits(generation, buffer, d)
            .then_some((generation, buffer))
    }

    /// Start animating display `d` to the newly fronted session (§5).
    ///
    /// Step 1 is the snapshot: the outgoing picture is copied out of the session's buffer, so the
    /// fade does not depend on that session drawing, answering or even living. logind already
    /// moved input when it changed the active session, so nothing typed during the fade reaches
    /// the outgoing session.
    fn begin_switch(&mut self, d: &str) {
        // The boot splash keeps turning until the incoming session draws: its first frame fades
        // in from it then (`fronted_frame`), rather than a still of the spinner sitting through
        // the wait.
        let splash_up = self.boot.is_some()
            && !self.shutting_down
            && self.plate_showing(d)
            && self.displays.get(d).is_some_and(|d| d.transition.is_none());
        if splash_up {
            if let Some((generation, buffer, fence)) = self.take_held(d) {
                self.fronted_frame(d, generation, buffer, fence, true);
            }
            return;
        }
        let from = self.snapshot_screen(d);
        self.cancel_transition(d);
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        // A frame of the outgoing session waiting to flip is that session's newest; it waits for
        // the switch back instead, still reserved.
        let queued = display.queued.take();
        display.transition = Some(Transition {
            from,
            to: None,
            pending: None,
            pending_watch: None,
            start: get_monotonic_time(),
            wait: None,
            from_live: self.outgoing_greeter,
        });
        if let Some(Next::Session {
            generation,
            buffer,
            fence,
            answer: true,
        }) = queued
        {
            self.hold(generation, d, buffer, fence);
        }

        if self.shutting_down {
            self.set_target(
                d,
                Target {
                    picture: Picture::Color(BLACK),
                    end: End::Hold,
                },
            );
            return;
        }

        match self.take_held(d) {
            // Its newest frame is already here: the session drew it, then was switched away
            // before it could be shown, and it draws nothing more until that frame is answered.
            Some((generation, buffer, fence)) => self.target_frame(d, generation, buffer, fence),
            None => self.arm_target_wait(d),
        }
    }

    /// The picture on display `d` now, as something a fade can start from.
    fn snapshot_screen(&mut self, d: &str) -> Picture {
        let Some(display) = self.displays.get(d) else {
            return plate_picture();
        };
        if matches!(display.on_screen, Shown::Plate | Shown::Splash { .. }) {
            return self.base_picture(d);
        }
        let display = &self.displays[d];
        let size = display.size();
        let compositor = self.compositor.as_mut().expect("only with a compositor");
        let captured = match &display.on_screen {
            Shown::Plate | Shown::Splash { .. } => unreachable!("handled above"),
            Shown::Session { dmabuf, .. } => compositor.snapshot(dmabuf),
            // Mid-fade: the blend on screen, redrawn, so the next fade starts where this one
            // stands instead of jumping to either end.
            Shown::Composite { alpha, .. } => match &display.transition {
                Some(Transition {
                    from, to: Some(to), ..
                }) => compositor.capture(size, from, &to.picture, *alpha),
                Some(Transition { from, .. }) => return from.clone(),
                None => return display.last_to.clone().unwrap_or_else(plate_picture),
            },
        };
        match captured {
            Ok(texture) => Picture::Texture(texture),
            Err(err) => {
                warn!("snapshotting {d}: {err:#}");
                plate_picture()
            }
        }
    }

    /// Drop `d`'s transition, returning a session frame it was holding to that session's `held`.
    fn cancel_transition(&mut self, d: &str) {
        let Some(transition) = self.displays.get_mut(d).and_then(|d| d.transition.take()) else {
            return;
        };
        if let Some(token) = transition.wait {
            self.handle.remove(token);
        }
        if let Some(token) = transition.pending_watch {
            self.handle.remove(token);
        }
        if let Some((generation, buffer, fence)) = transition.pending {
            self.hold(generation, d, buffer, fence);
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
            self.hold(generation, d, buffer, None);
        }
    }

    /// Keep `buffer` as `generation`'s newest unanswered frame for `d`, giving back the one it
    /// replaces.
    fn hold(&mut self, generation: u64, d: &str, buffer: u32, fence: Option<Rc<OwnedFd>>) {
        let Some(session) = self.sessions.get_mut(&generation) else {
            return;
        };
        if let Some((stale, _)) = session.held.insert(d.to_owned(), (buffer, fence)) {
            if stale != buffer {
                self.discard(generation, d, stale);
            }
        }
    }

    fn arm_target_wait(&mut self, d: &str) {
        // Not the compositor clock of `Synoik::timer_after`: the outer holds no `Synoik`.
        #[allow(clippy::disallowed_methods)]
        let timer = Timer::from_duration(self.switch_wait);
        let name = d.to_owned();
        let token = self
            .handle
            .insert_source(timer, move |_, _, outer: &mut Outer| {
                outer.on_target_wait(&name);
                TimeoutAction::Drop
            })
            .expect("inserting a timer");
        match self.displays.get_mut(d).and_then(|d| d.transition.as_mut()) {
            Some(transition) => transition.wait = Some(token),
            None => self.handle.remove(token),
        }
    }

    /// The incoming session has not drawn on `d` in time: fade to its last frame there, or the
    /// plate.
    fn on_target_wait(&mut self, d: &str) {
        let Some(transition) = self.displays.get_mut(d).and_then(|d| d.transition.as_mut()) else {
            return;
        };
        transition.wait = None;
        if transition.to.is_some() || transition.pending.is_some() {
            return;
        }
        let last = self.last_presented_on(d);
        debug!("switch target on {d} late; fading to {last:?}");
        let target = last.and_then(|(generation, buffer)| {
            let texture = self.import_frame(generation, buffer)?;
            Some(Target {
                picture: Picture::Texture(texture),
                end: End::Session {
                    generation,
                    buffer,
                    answer: false,
                },
            })
        });
        let target = match target {
            Some(target) => target,
            None => Target {
                picture: self.base_picture(d),
                end: End::Plate,
            },
        };
        self.set_target(d, target);
    }

    /// A frame of the incoming session becomes `d`'s target once its render is complete. Its
    /// present stays unanswered until it is flipped as it is at the end (§3.1: a target is not
    /// told it is on screen before it is).
    fn target_frame(&mut self, d: &str, generation: u64, buffer: u32, fence: Option<Rc<OwnedFd>>) {
        let signalled = fence.as_ref().is_none_or(|fd| fence_signalled(fd.as_fd()));
        let Some(transition) = self.displays.get_mut(d).and_then(|d| d.transition.as_mut()) else {
            return;
        };
        let wait = transition.wait.take();
        // A newer frame supersedes one still rendering: give that one back.
        let watch = transition.pending_watch.take();
        let stale = transition.pending.take();
        if let Some(token) = wait {
            self.handle.remove(token);
        }
        if let Some(token) = watch {
            self.handle.remove(token);
        }
        if let Some((g, stale, _)) = stale {
            self.discard(g, d, stale);
        }
        if signalled {
            self.target_ready(d, generation, buffer);
            return;
        }
        let fd = fence.clone().expect("unsignalled means fenced");
        let name = d.to_owned();
        let token = self
            .handle
            .insert_source(
                Generic::new(FdRef(fd), Interest::READ, Mode::OneShot),
                move |_, _, outer: &mut Outer| {
                    let transition = outer
                        .displays
                        .get_mut(&name)
                        .and_then(|d| d.transition.as_mut());
                    if let Some(transition) = transition {
                        if matches!(transition.pending, Some((g, b, _)) if g == generation && b == buffer)
                        {
                            transition.pending = None;
                            transition.pending_watch = None;
                            outer.target_ready(&name, generation, buffer);
                        }
                    }
                    Ok(PostAction::Remove)
                },
            )
            .expect("inserting a fence watch");
        let transition = self
            .displays
            .get_mut(d)
            .and_then(|d| d.transition.as_mut())
            .expect("checked above");
        transition.pending = Some((generation, buffer, fence));
        transition.pending_watch = Some(token);
    }

    fn target_ready(&mut self, d: &str, generation: u64, buffer: u32) {
        match self.import_frame(generation, buffer) {
            Some(texture) => self.set_target(
                d,
                Target {
                    picture: Picture::Texture(texture),
                    end: End::Session {
                        generation,
                        buffer,
                        answer: true,
                    },
                },
            ),
            None => {
                // Unusable as a picture; show it without the fade rather than not at all.
                self.cancel_transition(d);
                self.show(
                    d,
                    Next::Session {
                        generation,
                        buffer,
                        fence: None,
                        answer: true,
                    },
                );
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

    /// Make `target` what `d`'s fade goes to. The first target starts the fade's clock; a newer
    /// frame replacing it (the live `to`) keeps it, and gives back the frame it replaces if no
    /// blend has shown that one yet.
    fn set_target(&mut self, d: &str, target: Target) {
        // The outgoing greeter stops being live: the fade starts from its newest frame on screen.
        let live = self
            .displays
            .get_mut(d)
            .and_then(|d| d.transition.as_mut())
            .and_then(|t| t.from_live.take());
        if let Some(live) = live {
            let display = self.displays.get_mut(d).expect("has a transition");
            if let Some(Next::Session {
                generation,
                buffer,
                answer: true,
                ..
            }) = display
                .queued
                .take_if(|n| matches!(n, Next::Session { generation, .. } if *generation == live))
            {
                self.discard(generation, d, buffer);
            }
            let from = self.snapshot_screen(d);
            if let Some(transition) = self.displays.get_mut(d).and_then(|d| d.transition.as_mut()) {
                transition.from = from;
            }
        }
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        let Some(transition) = &mut display.transition else {
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
                    display.in_flight,
                    Some(Shown::Composite { samples: Some(s), .. }) if s == (generation, buffer)
                );
                if !sampled {
                    self.discard(generation, d, buffer);
                }
            }
            Some(_) => (),
        }
        if self.displays.get(d).is_some_and(|d| d.in_flight.is_none()) {
            self.advance(d);
        }
    }

    /// Draw and flip the next frame of `d`'s fade, or finish it. Runs once per flip completion,
    /// so the fade is paced by the display.
    fn advance(&mut self, d: &str) {
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        let Some(transition) = &display.transition else {
            return;
        };
        let Some(to) = &transition.to else {
            return;
        };
        let elapsed = get_monotonic_time().saturating_sub(transition.start);
        let t = (elapsed.as_secs_f32() / FADE.as_secs_f32()).clamp(0., 1.);
        let settled = matches!(display.on_screen, Shown::Composite { alpha, .. } if alpha >= 1.);
        if t >= 1. && (to.end != End::Hold || settled) {
            self.finish_transition(d);
            return;
        }
        // Ease out: most of the change early, settling into the incoming picture. A fade nothing
        // replaces draws its end too: its last frame is what stays on screen.
        let alpha = 1. - (1. - t).powi(3);
        let alpha = if t >= 1. { 1. } else { alpha };
        let busy = match &display.on_screen {
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
        let composed = compositor.compose(
            &mut display.canvas,
            &self.drm_fd,
            busy,
            &transition.from,
            &to.picture,
            alpha,
        );
        match composed {
            Ok((slot, fb, fence)) => self.flip(
                d,
                Next::Composite {
                    slot,
                    fb,
                    fence: fence.map(Rc::new),
                    alpha,
                    samples,
                },
            ),
            Err(err) => {
                warn!("drawing a transition frame on {d}: {err:#}");
                self.finish_transition(d);
            }
        }
    }

    /// Step 8: the fade on `d` is over, so put the incoming session's frame on screen as it is.
    fn finish_transition(&mut self, d: &str) {
        let Some(transition) = self.displays.get_mut(d).and_then(|d| d.transition.take()) else {
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
            debug!("a fade on {d} ended with frame {buffer} still rendering");
            self.hold(generation, d, buffer, fence);
        }
        let Some(to) = transition.to else {
            return;
        };
        if let Some(display) = self.displays.get_mut(d) {
            display.last_to = Some(to.picture);
        }
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
        self.show(d, end);

        // The incoming session drew while the fade was running on a stand-in.
        self.show_held(d, ended_on_plate);
    }

    /// A frame of the fronted session for `d`, outside a switch. It replaces the plate with a
    /// fade, and anything else as it is.
    fn fronted_frame(
        &mut self,
        d: &str,
        generation: u64,
        buffer: u32,
        fence: Option<Rc<OwnedFd>>,
        over_plate: bool,
    ) {
        if over_plate && self.compositor.is_some() {
            let from = self.base_picture(d);
            let Some(display) = self.displays.get_mut(d) else {
                return;
            };
            display.transition = Some(Transition {
                from,
                to: None,
                pending: None,
                pending_watch: None,
                start: get_monotonic_time(),
                wait: None,
                from_live: None,
            });
            self.target_frame(d, generation, buffer, fence);
        } else {
            self.show(
                d,
                Next::Session {
                    generation,
                    buffer,
                    fence,
                    answer: true,
                },
            );
        }
    }

    /// Show `next` on `d` now, or once the flip in flight lands.
    fn show(&mut self, d: &str, next: Next) {
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        if display.in_flight.is_some() {
            if let Some(Next::Session {
                generation,
                buffer,
                answer: true,
                ..
            }) = display.queued.replace(next)
            {
                self.discard(generation, d, buffer);
            }
        } else {
            self.flip(d, next);
        }
    }

    /// The picture of [`Self::draw_base`] on `d`, for a fade to start or end on.
    fn base_picture(&mut self, d: &str) -> Picture {
        if self.boot.is_none() {
            return plate_picture();
        }
        let Some(size) = self.displays.get(d).map(Display::size) else {
            return plate_picture();
        };
        let (_, frame) = self.splash_state();
        let splash = self.plymouth.server.splash().clone();
        let Some(compositor) = self.compositor.as_mut() else {
            return plate_picture();
        };
        match compositor.splash_picture(size, &splash, 1., frame) {
            Ok(texture) => Picture::Texture(texture),
            Err(err) => {
                warn!("drawing the splash on {d}: {err:#}");
                plate_picture()
            }
        }
    }

    /// Whether the plate is what `d` shows, or is about to.
    fn plate_showing(&self, d: &str) -> bool {
        let Some(display) = self.displays.get(d) else {
            return false;
        };
        match (&display.in_flight, &display.queued) {
            (_, Some(next)) => matches!(next, Next::Plate),
            (Some(shown), None) => matches!(shown, Shown::Plate | Shown::Splash { .. }),
            (None, None) => matches!(display.on_screen, Shown::Plate | Shown::Splash { .. }),
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
                let fd = session.fd.clone();
                let welcome = ToInner::Welcome {
                    version: protocol::VERSION,
                    render_node: self.render_node,
                    connectors: self.connector_infos(),
                };
                protocol::send(fd.as_fd(), &welcome, &[])?;
            }
            ToOuter::Configure { outputs, active } => {
                debug!(
                    "session {:?} configures {:?}{}",
                    session.logind_id,
                    outputs
                        .iter()
                        .map(|o| (&o.connector, o.mode.width, o.mode.height))
                        .collect::<Vec<_>>(),
                    if active { "" } else { ", blanked" }
                );
                let config = Some((outputs, active));
                let changed = session.config != config;
                session.config = config;
                if fronted && changed {
                    self.relayout_and_fill();
                }
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
                session.held.retain(|_, (b, _)| *b != id);
                session.last_presented.retain(|_, b| *b != id);
            }
            ToOuter::Present {
                output,
                buffer,
                fenced,
            } => {
                if !session.buffers.contains_key(&buffer) {
                    bail!("present of unknown buffer {buffer}");
                }
                let fence = fenced.then(|| Rc::new(fds.remove(0)));
                let d = output.as_str();
                // A frame for a display that is not lit, or not at the size it was drawn for — the
                // session drew it before a layout change reached it, or the outer could not apply
                // what the session asked — waits unanswered, as a background frame does.
                if !self.fits(generation, buffer, d) {
                    self.hold(generation, d, buffer, fence);
                    return Ok(());
                }
                let display = self.displays.get(d).expect("fits a lit display");
                let awaiting = display
                    .transition
                    .as_ref()
                    .map(|t| t.to.is_none() && t.pending.is_none());
                let feeds_fade = display.transition.as_ref().is_some_and(|t| {
                    t.pending.as_ref().is_some_and(|(g, ..)| *g == generation)
                        || matches!(
                            t.to,
                            Some(Target { end: End::Session { generation: g, .. }, .. })
                                if g == generation
                        )
                });
                let from_live = display
                    .transition
                    .as_ref()
                    .is_some_and(|t| t.from_live == Some(generation));
                match (fronted, awaiting) {
                    // The frame the switch to this session was waiting for (§5 step 6), or a newer
                    // one replacing it while the fade runs.
                    (true, Some(true)) => self.target_frame(d, generation, buffer, fence),
                    (true, Some(false)) if feeds_fade => {
                        self.target_frame(d, generation, buffer, fence)
                    }
                    (false, Some(true)) if from_live => self.show(
                        d,
                        Next::Session {
                            generation,
                            buffer,
                            fence,
                            answer: true,
                        },
                    ),
                    (true, None) => {
                        let over_plate = self.plate_showing(d);
                        self.fronted_frame(d, generation, buffer, fence, over_plate);
                    }
                    // A background session does not reach the screen, nor does a fronted one
                    // while the switch to it runs on another frame. Its newest frame is held
                    // unanswered; the one it supersedes is given back.
                    _ => self.hold(generation, d, buffer, fence),
                }
            }
        }
        Ok(())
    }

    /// Put `next` on display `d`. A session frame is gated on its fence in the kernel
    /// (`IN_FENCE_FD`), so it is never shown half-rendered and the outer never blocks on it.
    fn flip(&mut self, d: &str, next: Next) {
        let base = match next {
            Next::Plate => match self.draw_base(d) {
                Some(base) => Some(base),
                None => return,
            },
            _ => None,
        };
        let Some(display) = self.displays.get(d) else {
            return;
        };
        let size = display.size();
        let (fb_handle, shown, fence) = match next {
            Next::Plate => {
                let (fb, shown, fence) = base.expect("drawn above");
                (fb, shown, fence.map(Rc::new))
            }
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
            handle: display.surface.plane(),
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
        match display.surface.page_flip([plane], true) {
            Ok(()) => {
                self.displays.get_mut(d).expect("looked up above").in_flight = Some(shown);
            }
            Err(err) => {
                warn!("flip on {d} refused: {err}");
                match shown {
                    Shown::Session {
                        generation,
                        buffer,
                        answer: true,
                        ..
                    } => self.discard(generation, d, buffer),
                    Shown::Session { answer: false, .. } => (),
                    // No flip completion will come to draw the next frame: end the fade now.
                    Shown::Composite { .. } => self.finish_transition(d),
                    Shown::Plate | Shown::Splash { .. } => (),
                }
            }
        }
    }

    /// Whether `buffer` is still spoken for on `d` — held, queued, awaited by a fade, or what the
    /// session was last told is on screen — so it must not be given back.
    fn reserved(&self, generation: u64, d: &str, buffer: u32) -> bool {
        let session = self.sessions.get(&generation);
        let display = self.displays.get(d);
        session.is_some_and(|s| {
            s.held.get(d).is_some_and(|(b, _)| *b == buffer)
                || s.last_presented.get(d) == Some(&buffer)
        }) || display.is_some_and(|display| {
            matches!(
                display.queued,
                Some(Next::Session { generation: g, buffer: b, .. }) if g == generation && b == buffer
            ) || display.transition.as_ref().is_some_and(|t| {
                matches!(t.pending, Some((g, b, _)) if g == generation && b == buffer)
                    || matches!(
                        t.to,
                        Some(Target { end: End::Session { generation: g, buffer: b, .. }, .. })
                            if g == generation && b == buffer
                    )
            })
        })
    }

    /// Tell `generation` its `buffer` reached display `d`.
    fn presented(
        &mut self,
        generation: u64,
        d: &str,
        buffer: u32,
        meta: Option<&DrmEventMetadata>,
    ) {
        let refresh = self
            .displays
            .get(d)
            .map_or(0, |d| d.refresh.as_nanos() as u64);
        let Some(session) = self.sessions.get_mut(&generation) else {
            return;
        };
        session.last_presented.insert(d.to_owned(), buffer);
        let (sequence, time_ns) = match meta {
            Some(DrmEventMetadata {
                sequence,
                time: DrmEventTime::Monotonic(time),
            }) => (*sequence, time.as_nanos() as u64),
            Some(DrmEventMetadata { sequence, .. }) => (*sequence, 0),
            None => (0, 0),
        };
        let msg = ToInner::Presented {
            output: d.to_owned(),
            buffer,
            sequence,
            time_ns,
            refresh_ns: refresh,
        };
        if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
            warn!("reporting a flip to a session: {err}");
        }
    }

    fn discard(&self, generation: u64, d: &str, buffer: u32) {
        if let Some(session) = self.sessions.get(&generation) {
            debug!(
                "discarding buffer {buffer} on {d} of {:?}",
                session.logind_id
            );
            let msg = ToInner::Discarded {
                output: d.to_owned(),
                buffer,
            };
            if let Err(err) = protocol::send(session.fd.as_fd(), &msg, &[]) {
                warn!("telling a session buffer {buffer} was discarded: {err}");
            }
        }
    }

    fn on_flip(&mut self, crtc: crtc::Handle, meta: Option<DrmEventMetadata>) {
        let Some(d) = self
            .displays
            .iter()
            .find(|(_, display)| display.crtc == crtc)
            .map(|(name, _)| name.clone())
        else {
            debug!("flip completion on a CRTC no display drives");
            return;
        };
        let d = d.as_str();
        let Some(shown) = self.displays.get_mut(d).and_then(|d| d.in_flight.take()) else {
            debug!("flip completion on {d} with nothing in flight");
            return;
        };

        match &shown {
            Shown::Session {
                buffer,
                generation,
                answer: true,
                ..
            } => self.presented(*generation, d, *buffer, meta.as_ref()),
            // A blend of the incoming session's frame is that frame on screen, as far as the
            // session's pacing goes: answer it, so it draws its next one.
            Shown::Composite {
                samples: Some((generation, buffer)),
                ..
            } => {
                let (generation, buffer) = (*generation, *buffer);
                // `Some(unanswered)` while it is still the target.
                let target = match self
                    .displays
                    .get_mut(d)
                    .and_then(|d| d.transition.as_mut())
                    .and_then(|t| t.to.as_mut())
                {
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
                    Some(true) => self.presented(generation, d, buffer, meta.as_ref()),
                    Some(false) => (),
                    // Replaced while this blend was reading it (`set_target`): free now — unless
                    // it went back to the session meanwhile (a cancelled fade holds its target).
                    None if !self.reserved(generation, d, buffer) => {
                        self.discard(generation, d, buffer)
                    }
                    None => (),
                }
            }
            _ => (),
        }
        let session_up = matches!(shown, Shown::Session { .. });
        let display = self.displays.get_mut(d).expect("found above");
        // Dropping the previous frame here, not earlier: it was on the screen until this moment.
        display.on_screen = shown;
        if session_up {
            self.end_boot_splash();
        }
        let display = self.displays.get_mut(d).expect("found above");

        if std::mem::take(&mut display.relayout_pending) {
            // The layout changed under the flip; it can be applied now.
            self.relayout_and_fill();
        }
        // Turned off, or a frame already on its way at the new layout.
        let Some(display) = self.displays.get_mut(d) else {
            return;
        };
        if display.in_flight.is_some() {
            return;
        }
        if std::mem::take(&mut display.switch_pending) {
            // Now that the screen has settled, take the switch from what it shows.
            self.begin_switch(d);
        } else if let Some(next) = display.queued.take() {
            self.flip(d, next);
        } else if display.transition.is_some() {
            self.advance(d);
        }
    }
}

/// Wait for a render to complete, bounded: a stuck GPU should not keep the display dark forever.
fn wait_fence(fd: BorrowedFd<'_>) {
    let mut pfd = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd.
    if unsafe { libc::poll(&mut pfd, 1, 1000) } != 1 {
        warn!("a splash render did not complete in time");
    }
}

/// Light `display` with `fb`. A modeset, so a commit rather than a flip.
fn commit_frame(name: &str, display: &Display, fb: framebuffer::Handle) -> anyhow::Result<()> {
    let size = display.size();
    let plane = PlaneState {
        handle: display.surface.plane(),
        config: Some(PlaneConfig {
            src: Rectangle::from_size((size.0 as f64, size.1 as f64).into()),
            dst: Rectangle::from_size((size.0 as i32, size.1 as i32).into()),
            transform: Transform::Normal,
            alpha: 1.0,
            damage_clips: None,
            fb,
            fence: None,
            hotspot: None,
        }),
    };
    // No page-flip event wanted: nothing waits on the plate, and an event here would land as a
    // flip completion with nothing in flight.
    display
        .surface
        .commit([plane], false)
        .with_context(|| format!("modesetting {name}"))?;
    info!("{name} lit at {}x{}", size.0, size.1);
    Ok(())
}

/// The plate at `size`: a flat buffer and its framebuffer.
fn make_plate(
    drm_fd: &DrmDeviceFd,
    size: (u16, u16),
) -> anyhow::Result<(DumbBuffer, framebuffer::Handle)> {
    let (w, h) = size;
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
    Ok((plate_buffer, plate))
}

/// A mode as a session sees it; the refresh counted as `smithay::output::Mode` counts it, so a
/// session's mode selection compares rates as the tty backend's does.
fn mode_info(m: &DrmMode) -> ModeInfo {
    ModeInfo {
        width: m.size().0,
        height: m.size().1,
        refresh_mhz: smithay::output::Mode::from(*m).refresh as u32,
        preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
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

pub(super) struct PeerCred {
    pub pid: i32,
    pub uid: u32,
}

pub(super) fn peer_cred(fd: BorrowedFd<'_>) -> io::Result<PeerCred> {
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
