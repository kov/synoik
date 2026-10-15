// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The initramfs splash (`docs/fork/boot-splash.md` §7): plymouthd for the initramfs. It holds the
//! card from the boot's first DRM frame, draws the splash into dumb buffers, answers plymouth's
//! clients, and lives on across switch_root (as `synoik-system.service`, its `argv[0]` marked `@`)
//! until the real root restarts the unit into the outer. At that TERM it leaves the splash in the
//! fd store, beside the card and the listener, for the outer to carry on.

#[macro_use]
extern crate tracing;

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use drm::buffer::DrmFourcc;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{connector, crtc, framebuffer, Device as _, Mode, ModeTypeFlags, PageFlipFlags};
use synoik_splash::handoff::{self, State};
use synoik_splash::layout::{self, Size};
use synoik_splash::paint::{self, Canvas, Theme};
use synoik_splash::plymouth::{self, Event, Server};

/// The card, as the drm crate wants it.
struct Card(OwnedFd);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// Present while this program is the unit's process: what the real root's handover runs on.
const MARKER: &str = "/run/synoik-system/initrd-splash";

/// How often to look for the card while it is not there yet.
const CARD_POLL: Duration = Duration::from_millis(20);

/// How long the TERM handoff waits for systemd to take the splash state.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(2);

struct Buf {
    _db: DumbBuffer,
    fb: framebuffer::Handle,
    /// Mapped once, at setup: once the real root loads its SELinux policy, `kernel_t` may no
    /// longer map the card (the initramfs's devtmpfs label), but a mapping it has stays good.
    map: Mapping,
    stride: usize,
    /// The splash revision last painted in full here; a turn of the spinner repaints only its
    /// area over that.
    drawn: Option<u64>,
}

struct Display {
    crtc: crtc::Handle,
    conn: connector::Handle,
    mode: Mode,
    size: Size,
    bufs: [Buf; 2],
    /// The buffer on screen, once one is.
    front: Option<usize>,
    flip_pending: bool,
    /// What the screen shows: (revision, spinner frame).
    shows: Option<(u64, usize)>,
}

struct Splash {
    server: Server,
    theme: Theme,
    card: Option<Card>,
    recovered: bool,
    displays: Vec<Display>,
    connections: HashMap<u64, ()>,
    /// When the spinner's turn began, on the monotonic clock.
    start: Duration,
    /// The frame a frozen spinner keeps.
    frozen: Option<usize>,
    revision: u64,
}

fn monotonic() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a timespec of ours.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn under_systemd() -> bool {
    std::env::var_os("NOTIFY_SOCKET").is_some()
}

fn main() {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(io::stderr)
        .init();
    if let Err(err) = run() {
        error!("{err}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let signals = block_signals()?;

    let mut drm = None;
    let mut listener = None;
    let mut resume = None;
    if let Ok(fds) = sd_notify::listen_fds_with_names() {
        for (raw, name) in fds {
            // SAFETY: systemd passed this descriptor to us; it is ours to own from here on.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            match name.as_str() {
                handoff::FD_DRM => drm = Some(Card(fd)),
                handoff::FD_PLYMOUTH => listener = Some(UnixListener::from(fd)),
                handoff::FD_SPLASH => resume = State::from_fd(fd.as_fd()).ok(),
                _ => (),
            }
        }
    }

    let mut splash = Splash {
        // plymouthd answers root only (`ply-boot-server.c:265-285`).
        server: Server::new(0),
        theme: Theme::load(Path::new(layout::THEME_DIR)),
        recovered: drm.is_some(),
        card: drm,
        displays: Vec::new(),
        connections: HashMap::new(),
        start: monotonic(),
        frozen: None,
        revision: 0,
    };
    match resume {
        Some(state) => {
            splash.start = state.spinner_start;
            splash.frozen = state.frozen_frame;
            splash.server.restore(state.splash);
        }
        None => splash.server.show(),
    }
    splash.server.take_events();

    let bound = match listener {
        Some(listener) => splash.server.adopt(listener),
        None => splash.server.try_bind(plymouth::SOCKET_NAME).inspect(|()| {
            if let (true, Some(fd)) = (under_systemd(), splash.server.listener_fd()) {
                if let Err(err) = handoff::store(handoff::FD_PLYMOUTH, fd) {
                    warn!("could not put plymouth's listener in the fd store: {err}");
                }
            }
        }),
    };
    match bound {
        Ok(()) => {
            if let Err(err) = fs::create_dir_all("/run/plymouth") {
                warn!("/run/plymouth: {err}");
            }
            splash
                .server
                .set_pid_file(std::path::PathBuf::from(plymouth::PID_FILE));
            info!("answering plymouth's clients");
        }
        Err(err) => warn!("not answering plymouth's clients: {err}"),
    }
    if let Some(dir) = Path::new(MARKER).parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Err(err) = fs::write(MARKER, b"") {
        warn!("{MARKER}: {err}");
    }
    if under_systemd() {
        sd_notify::notify(&[sd_notify::NotifyState::Ready])?;
    }

    loop {
        if splash.card.is_none() {
            splash.open_card();
        }
        if !splash.displays.is_empty() && splash.displays.iter().all(|d| d.front.is_none()) {
            splash.light();
        }
        splash.redraw();

        let mut fds = vec![libc::pollfd {
            fd: signals.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(fd) = splash.server.listener_fd() {
            fds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(card) = &splash.card {
            fds.push(libc::pollfd {
                fd: card.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let ids: Vec<u64> = splash.connections.keys().copied().collect();
        for &id in &ids {
            if let Some(fd) = splash.server.connection_fd(id) {
                fds.push(libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
        }
        let timeout = splash.timeout();
        // SAFETY: pollfds of ours.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }

        if fds[0].revents != 0 {
            drain(signals.as_fd());
            splash.hand_off();
            return Ok(());
        }
        let ready = |fd: i32| fds.iter().any(|p| p.fd == fd && p.revents != 0);
        if splash
            .server
            .listener_fd()
            .is_some_and(|fd| ready(fd.as_raw_fd()))
        {
            for id in splash.server.accept() {
                splash.connections.insert(id, ());
            }
        }
        if splash
            .card
            .as_ref()
            .is_some_and(|c| ready(c.as_fd().as_raw_fd()))
        {
            splash.on_card_readable();
        }
        for id in ids {
            let readable = splash
                .server
                .connection_fd(id)
                .is_some_and(|fd| ready(fd.as_raw_fd()));
            if readable {
                splash.server.on_readable(id);
            }
        }
        splash.on_plymouth();
    }
}

impl Splash {
    /// The spinner frame to draw now.
    fn spinner_frame(&self) -> usize {
        if !self.server.splash().shown {
            return 0;
        }
        if let Some(frame) = self.frozen {
            return frame;
        }
        let frames = self.theme.spinner.len().max(1);
        layout::spinner_frame(monotonic().saturating_sub(self.start), frames)
    }

    /// How long poll may sleep, in ms: until the spinner's next frame, or the next look for the
    /// card.
    fn timeout(&self) -> i32 {
        if self.card.is_none() {
            return CARD_POLL.as_millis() as i32;
        }
        let turning = self.server.splash().shown && self.frozen.is_none();
        if !turning || self.displays.is_empty() {
            return -1;
        }
        let interval = layout::SPINNER_FRAME_INTERVAL.as_nanos();
        let elapsed = monotonic().saturating_sub(self.start).as_nanos();
        let left = interval - elapsed % interval;
        left.div_ceil(1_000_000) as i32
    }

    fn open_card(&mut self) {
        let path = c"/dev/dri/card0";
        // SAFETY: a static path.
        let raw = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if raw < 0 {
            return;
        }
        // SAFETY: just opened, and ours.
        let card = Card(unsafe { OwnedFd::from_raw_fd(raw) });
        info!("opened /dev/dri/card0");
        if under_systemd() {
            if let Err(err) = handoff::store(handoff::FD_DRM, card.as_fd()) {
                warn!("could not put the DRM fd in the fd store: {err}");
            }
        }
        self.card = Some(card);
        self.setup_displays();
    }

    /// Every connected connector, at its preferred mode, with its own CRTC and two buffers.
    fn setup_displays(&mut self) {
        let Some(card) = &self.card else { return };
        if self.recovered {
            close_inherited_framebuffers(card);
        }
        let res = match card.resource_handles() {
            Ok(res) => res,
            Err(err) => {
                warn!("reading the card's resources: {err}");
                return;
            }
        };
        let mut taken = Vec::new();
        for &handle in res.connectors() {
            // A master's probe: no in-kernel client needs to have probed first (§7.3).
            let Ok(conn) = card.get_connector(handle, true) else {
                continue;
            };
            if conn.state() != connector::State::Connected || conn.modes().is_empty() {
                continue;
            }
            let mode = *conn
                .modes()
                .iter()
                .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                .unwrap_or(&conn.modes()[0]);
            let Some(crtc) = pick_crtc(card, &res, &conn, &taken) else {
                warn!("no CRTC for connector {handle:?}");
                continue;
            };
            taken.push(crtc);
            match Display::new(card, crtc, handle, mode) {
                Ok(display) => {
                    let (w, h) = mode.size();
                    info!("lighting {:?} at {w}x{h}", conn.interface());
                    self.displays.push(display);
                }
                Err(err) => warn!("setting up a display: {err}"),
            }
        }
    }

    /// The first frame on every display: a modeset, as plymouthd's first frame is.
    fn light(&mut self) {
        let state = (self.revision, self.spinner_frame());
        let splash = self.server.splash().clone();
        let Some(card) = &self.card else { return };
        for display in &mut self.displays {
            let items = paint::items(&splash, display.size, &self.theme, state.1);
            if let Err(err) = display.paint(0, self.revision, &items, &self.theme) {
                warn!("painting the splash: {err}");
                continue;
            }
            if let Err(err) = display.modeset(card) {
                warn!("lighting a display: {err}");
                continue;
            }
            display.front = Some(0);
            display.shows = Some(state);
        }
    }

    /// Flip every idle display whose picture is out of date.
    fn redraw(&mut self) {
        let state = (self.revision, self.spinner_frame());
        let splash = self.server.splash().clone();
        let Some(card) = &self.card else { return };
        for display in &mut self.displays {
            let Some(front) = display.front else { continue };
            if display.flip_pending || display.shows == Some(state) {
                continue;
            }
            let back = 1 - front;
            let items = paint::items(&splash, display.size, &self.theme, state.1);
            if let Err(err) = display.paint(back, self.revision, &items, &self.theme) {
                warn!("painting the splash: {err}");
                continue;
            }
            match card.page_flip(
                display.crtc,
                display.bufs[back].fb,
                PageFlipFlags::EVENT,
                None,
            ) {
                Ok(()) => {
                    display.flip_pending = true;
                    display.shows = Some(state);
                }
                Err(err) => warn!("flipping: {err}"),
            }
        }
    }

    fn on_card_readable(&mut self) {
        let Some(card) = &self.card else { return };
        let events = match card.receive_events() {
            Ok(events) => events,
            Err(err) => {
                warn!("reading DRM events: {err}");
                return;
            }
        };
        for event in events {
            if let drm::control::Event::PageFlip(flip) = event {
                if let Some(d) = self.displays.iter_mut().find(|d| d.crtc == flip.crtc) {
                    d.flip_pending = false;
                    d.front = d.front.map(|f| 1 - f);
                }
            }
        }
    }

    /// Act on what plymouth's clients changed.
    fn on_plymouth(&mut self) {
        loop {
            // No dialog until step 5: the client asks some other way.
            while self.server.splash().password_prompt.is_some() {
                self.server.answer_password(None);
            }
            let events = self.server.take_events();
            if events.is_empty() {
                break;
            }
            for event in events {
                match event {
                    Event::Changed => self.on_splash_changed(),
                    Event::Quit { retain } => {
                        info!("plymouth quit (retain: {retain})");
                        self.server.finish_quit();
                    }
                }
            }
        }
        for (id, stream) in self.server.take_closing() {
            self.connections.remove(&id);
            drop(stream);
        }
    }

    fn on_splash_changed(&mut self) {
        let frame = self.spinner_frame();
        self.revision += 1;
        match (self.server.splash().frozen, self.frozen.is_some()) {
            (true, false) => self.frozen = Some(frame),
            // `$` after `H` starts the spinner over (`ply_throbber_start`).
            (false, true) => {
                self.frozen = None;
                self.start = monotonic();
            }
            _ => (),
        }
    }

    /// The real root is restarting the unit into the outer (§7.2): leave it the splash. The card
    /// and the listener are in the fd store already; closing them here changes nothing on screen.
    fn hand_off(&mut self) {
        info!("handing the splash over");
        if under_systemd() {
            let state = State {
                splash: self.server.splash().clone(),
                spinner_start: self.start,
                frozen_frame: self.frozen,
            };
            let stored = state
                .to_memfd()
                .and_then(|fd| handoff::store(handoff::FD_SPLASH, fd.as_fd()))
                .and_then(|()| handoff::barrier(BARRIER_TIMEOUT));
            if let Err(err) = stored {
                warn!("could not leave the splash in the fd store: {err}");
            }
        }
        self.server.release_pid_file();
        let _ = fs::remove_file(MARKER);
    }
}

impl Display {
    fn new(
        card: &Card,
        crtc: crtc::Handle,
        conn: connector::Handle,
        mode: Mode,
    ) -> io::Result<Self> {
        let (w, h) = mode.size();
        let buf = || -> io::Result<Buf> {
            let db = card.create_dumb_buffer((w.into(), h.into()), DrmFourcc::Xrgb8888, 32)?;
            let fb = card.add_framebuffer(&db, 24, 32)?;
            let stride = drm::buffer::Buffer::pitch(&db) as usize;
            let map = Mapping::new(card, &db, stride * usize::from(h))?;
            Ok(Buf {
                _db: db,
                fb,
                map,
                stride,
                drawn: None,
            })
        };
        let bufs = [buf()?, buf()?];
        Ok(Self {
            crtc,
            conn,
            mode,
            size: Size::from((i32::from(w), i32::from(h))),
            bufs,
            front: None,
            flip_pending: false,
            shows: None,
        })
    }

    /// Paint `items` into buffer `index`: in full if it last held another revision, else only the
    /// spinner.
    fn paint(
        &mut self,
        index: usize,
        revision: u64,
        items: &[layout::Item],
        theme: &Theme,
    ) -> io::Result<()> {
        let size = self.size;
        let buf = &mut self.bufs[index];
        let full = buf.drawn != Some(revision);
        let mut canvas = Canvas {
            size,
            stride: buf.stride,
            bytes: buf.map.bytes(),
        };
        if full {
            paint::paint(&mut canvas, items, theme);
        } else {
            paint::paint_spinner(&mut canvas, items, theme);
        }
        buf.drawn = Some(revision);
        Ok(())
    }

    fn modeset(&self, card: &Card) -> io::Result<()> {
        card.set_crtc(
            self.crtc,
            Some(self.bufs[0].fb),
            (0, 0),
            &[self.conn],
            Some(self.mode),
        )
    }
}

/// A dumb buffer's pixels, mapped for as long as the program runs.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    fn new(card: &Card, db: &DumbBuffer, len: usize) -> io::Result<Self> {
        #[repr(C)]
        struct DrmModeMapDumb {
            handle: u32,
            pad: u32,
            offset: u64,
        }
        // DRM_IOWR(0xB3, struct drm_mode_map_dumb)
        const DRM_IOCTL_MODE_MAP_DUMB: libc::c_ulong = 0xC010_64B3;
        let mut arg = DrmModeMapDumb {
            handle: u32::from(drm::buffer::Buffer::handle(db)),
            pad: 0,
            offset: 0,
        };
        let fd = card.as_fd().as_raw_fd();
        // SAFETY: the ioctl's own argument struct, on our card.
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &raw mut arg) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a shared mapping of the buffer the kernel just gave the offset of.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                arg.offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `len` bytes mapped at `ptr` by `new`, and only reached through `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// A CRTC for `conn`: the one already driving it, else a free one it can use.
fn pick_crtc(
    card: &Card,
    res: &drm::control::ResourceHandles,
    conn: &connector::Info,
    taken: &[crtc::Handle],
) -> Option<crtc::Handle> {
    if let Some(crtc) = conn
        .current_encoder()
        .and_then(|e| card.get_encoder(e).ok())
        .and_then(|e| e.crtc())
        .filter(|c| !taken.contains(c))
    {
        return Some(crtc);
    }
    conn.encoders().iter().find_map(|&e| {
        let enc = card.get_encoder(e).ok()?;
        res.filter_crtcs(enc.possible_crtcs())
            .into_iter()
            .find(|c| !taken.contains(c))
    })
}

/// Let go of framebuffers a previous splash made on the inherited card without taking them off
/// the screen (`CLOSEFB`, as the outer does on a restart).
fn close_inherited_framebuffers(card: &Card) {
    let Ok(res) = card.resource_handles() else {
        return;
    };
    for &fb in res.framebuffers() {
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
        // SAFETY: the ioctl's own argument struct, on our card.
        if unsafe {
            libc::ioctl(
                card.as_fd().as_raw_fd(),
                DRM_IOCTL_MODE_CLOSEFB,
                &raw mut arg,
            )
        } < 0
        {
            warn!(
                "closing inherited framebuffer {fb:?}: {}",
                io::Error::last_os_error()
            );
        }
    }
}

/// TERM and INT come through a signalfd: the handoff runs from the loop, not a handler.
fn block_signals() -> io::Result<OwnedFd> {
    // SAFETY: a sigset of ours, filled by sigemptyset/sigaddset.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        libc::sigaddset(&raw mut set, libc::SIGTERM);
        libc::sigaddset(&raw mut set, libc::SIGINT);
        if libc::sigprocmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::signalfd(-1, &raw const set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn drain(fd: BorrowedFd<'_>) {
    let mut buf = [0u8; 128];
    // SAFETY: a buffer of ours.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}
