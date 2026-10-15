// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The initramfs splash (`docs/fork/boot-splash.md` §7): plymouthd for the initramfs. It holds the
//! card from the boot's first DRM frame, draws the splash into dumb buffers, answers plymouth's
//! clients, and lives on across switch_root (as `synoik-system.service`, its `argv[0]` marked `@`)
//! until the real root restarts the unit into the outer. At that TERM it leaves the splash in the
//! fd store, beside the card and the listener, for the outer to carry on.
//!
//! An initramfs without systemd (initramfs-tools) starts it from a boot script instead, and the
//! real root's unit fetches the same three descriptors from it over [`handoff::HANDOVER_SOCKET`]
//! with `synoik-splash --hand-over` (§7.6).

#[macro_use]
extern crate tracing;

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use drm::buffer::DrmFourcc;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{connector, crtc, framebuffer, Device as _, Mode, ModeTypeFlags, PageFlipFlags};
use synoik_splash::handoff::{self, State};
use synoik_splash::keyboard::{self, Entry, Keyboard};
use synoik_splash::layout::{self, Dialog, Size};
use synoik_splash::paint::{self, Canvas, Frame, Theme};
use synoik_splash::plymouth::{self, Event, Server};
use synoik_splash::text::{self, Fonts};

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

/// How often to probe the card's connectors again while none is lit, between hotplug events: a
/// forced probe reads EDID, so not at the card's rate.
const CONNECTOR_POLL: Duration = Duration::from_secs(1);

/// How long the TERM handoff waits for systemd to take the splash state.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `--hand-over` waits for the splash's descriptors.
const HANDOVER_TIMEOUT: Duration = Duration::from_secs(5);

struct Buf {
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
    fonts: Fonts,
    /// Open from the start, so the keyboards are open before the real root's SELinux policy
    /// loads (§7.1); armed while a password prompt is up.
    keyboard: Option<Keyboard>,
    entry: Entry,
    card: Option<Card>,
    recovered: bool,
    displays: Vec<Display>,
    /// The kernel's uevents, read while the card has no display lit: a connector may come up after
    /// the card does (a VM's host window), and plymouthd waits for its hotplug event too.
    uevents: Option<OwnedFd>,
    /// When the connectors were last probed, on the monotonic clock.
    probed: Duration,
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
    // At image build (`90synoik`): the system keymap, compiled for the image to carry.
    if std::env::args().nth(1).as_deref() == Some("--compile-keymap") {
        match keyboard::compile_system_keymap() {
            Some(keymap) => print!("{keymap}"),
            None => {
                eprintln!("synoik-splash: the system keymap does not compile");
                std::process::exit(1);
            }
        }
        return;
    }
    // From the real root's unit, before the outer starts: the splash's descriptors, into the
    // unit's fd store (§7.6).
    let hand_over = std::env::args().nth(1).as_deref() == Some("--hand-over");
    tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(io::stderr)
        .init();
    let result = if hand_over { fetch_splash() } else { run() };
    if let Err(err) = result {
        error!("{err}");
        std::process::exit(1);
    }
}

/// Take the running splash's descriptors into this unit's fd store; nothing when no splash is
/// waiting, as on a boot whose initramfs started the unit itself.
fn fetch_splash() -> io::Result<()> {
    let addr = SocketAddr::from_abstract_name(handoff::HANDOVER_SOCKET)?;
    let stream = match UnixStream::connect_addr(&addr) {
        Ok(stream) => stream,
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => return Ok(()),
        Err(err) => return Err(err),
    };
    // Anyone may bind an abstract name: only root's splash holds the card.
    let uid = handoff::peer_uid(stream.as_fd())?;
    if uid != 0 {
        return Err(io::Error::other(format!(
            "the splash socket is uid {uid}'s"
        )));
    }
    stream.set_read_timeout(Some(HANDOVER_TIMEOUT))?;
    let fds = handoff::receive_fds(stream.as_fd())?;
    for (name, fd) in &fds {
        handoff::store(name, fd.as_fd())?;
    }
    handoff::barrier(BARRIER_TIMEOUT)?;
    info!(
        "took over the splash's {}",
        fds.iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn run() -> io::Result<()> {
    let signals = block_signals()?;
    // Without systemd here, the real root asks for the descriptors over this.
    let handover = if under_systemd() {
        None
    } else {
        let addr = SocketAddr::from_abstract_name(handoff::HANDOVER_SOCKET)?;
        let listener = UnixListener::bind_addr(&addr)?;
        listener.set_nonblocking(true)?;
        Some(listener)
    };

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
        fonts: Fonts::load(&[Path::new(text::FONT_FILE)]),
        keyboard: open_keyboard(),
        entry: Entry::default(),
        recovered: drm.is_some(),
        card: drm,
        displays: Vec::new(),
        uevents: open_uevents()
            .inspect_err(|err| warn!("not listening for hotplug events: {err}"))
            .ok(),
        probed: Duration::ZERO,
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
    if under_systemd() {
        if let Some(dir) = Path::new(MARKER).parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Err(err) = fs::write(MARKER, b"") {
            warn!("{MARKER}: {err}");
        }
        sd_notify::notify(&[sd_notify::NotifyState::Ready])?;
    }

    loop {
        if splash.card.is_none() {
            splash.open_card();
        } else if splash.displays.is_empty()
            && monotonic().saturating_sub(splash.probed) >= CONNECTOR_POLL
        {
            splash.setup_displays();
        }
        if !splash.displays.is_empty() && splash.displays.iter().all(|d| d.front.is_none()) {
            splash.light();
            splash.uevents = None;
        }
        splash.redraw();

        let mut fds = vec![libc::pollfd {
            fd: signals.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(listener) = &handover {
            fds.push(libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
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
        if let Some(uevents) = &splash.uevents {
            fds.push(libc::pollfd {
                fd: uevents.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(keyboard) = &splash.keyboard {
            fds.push(libc::pollfd {
                fd: keyboard.fd().as_raw_fd(),
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
        if let Some(listener) = handover.as_ref().filter(|l| ready(l.as_raw_fd())) {
            if let Ok((stream, _)) = listener.accept() {
                match handoff::peer_uid(stream.as_fd()) {
                    Ok(0) => match splash.hand_over(&stream) {
                        Ok(()) => return Ok(()),
                        Err(err) => warn!("could not hand the splash over: {err}"),
                    },
                    Ok(uid) => warn!("refusing the splash to uid {uid}"),
                    Err(err) => warn!("handover: {err}"),
                }
            }
        }
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
        if let Some(uevents) = splash.uevents.as_ref().filter(|u| ready(u.as_raw_fd())) {
            if drm_hotplug(uevents.as_fd()) && splash.displays.is_empty() {
                splash.setup_displays();
            }
        }
        if splash
            .keyboard
            .as_ref()
            .is_some_and(|k| ready(k.fd().as_raw_fd()))
        {
            splash.on_keyboard();
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
        // Nothing turns: no splash, or a password dialog in the spinner's place.
        let splash = self.server.splash();
        if !splash.shown || splash.password_prompt.is_some() {
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
        if self.displays.is_empty() {
            return CONNECTOR_POLL.as_millis() as i32;
        }
        let splash = self.server.splash();
        let turning = splash.shown && splash.password_prompt.is_none() && self.frozen.is_none();
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
        self.probed = monotonic();
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
        let dialog = self.dialog();
        let Some(card) = &self.card else { return };
        for display in &mut self.displays {
            let frame = paint::items(
                &splash,
                display.size,
                &self.theme,
                state.1,
                &dialog,
                Some(&mut self.fonts),
            );
            if let Err(err) = display.paint(0, self.revision, &frame, &self.theme) {
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
        let dialog = self.dialog();
        let Some(card) = &self.card else { return };
        for display in &mut self.displays {
            let Some(front) = display.front else { continue };
            if display.flip_pending || display.shows == Some(state) {
                continue;
            }
            let back = 1 - front;
            let frame = paint::items(
                &splash,
                display.size,
                &self.theme,
                state.1,
                &dialog,
                Some(&mut self.fonts),
            );
            if let Err(err) = display.paint(back, self.revision, &frame, &self.theme) {
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
        self.follow_prompt();
    }

    /// Type into the dialog while a password prompt is up, and only then. Without a keyboard
    /// the dialog still shows: NO_ANSWER would leave the asking unit waiting unseen (§3.2).
    fn follow_prompt(&mut self) {
        let prompting = self.server.splash().password_prompt.is_some();
        if prompting {
            if self.keyboard.is_none() {
                self.keyboard = open_keyboard();
            }
            if let Some(keyboard) = self.keyboard.as_mut().filter(|k| !k.is_armed()) {
                info!("asking for a password");
                keyboard.arm();
                self.revision += 1;
            }
        } else if let Some(keyboard) = self.keyboard.as_mut().filter(|k| k.is_armed()) {
            keyboard.disarm();
            self.entry.clear();
            self.revision += 1;
        }
    }

    /// What the keyboard says about the password being typed.
    fn dialog(&self) -> Dialog {
        Dialog {
            bullets: self.entry.bullets(),
            caps_lock: self.keyboard.as_ref().is_some_and(Keyboard::caps_lock),
            keymap: self.keyboard.as_ref().and_then(Keyboard::layout_name),
        }
    }

    fn on_keyboard(&mut self) {
        let Some(keyboard) = &mut self.keyboard else {
            return;
        };
        let (keys, leds) = keyboard.dispatch();
        // Unarmed, caps lock changes nothing on screen.
        let mut changed = leds && keyboard.is_armed();
        for key in keys {
            changed = true;
            if let Some(password) = self.entry.apply(key) {
                self.server.answer_password(Some(password));
            }
        }
        if changed {
            self.revision += 1;
        }
        self.on_plymouth();
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

    /// The splash as it stands, for the outer to resume.
    fn state(&self) -> State {
        State {
            splash: self.server.splash().clone(),
            spinner_start: self.start,
            frozen_frame: self.frozen,
        }
    }

    /// The real root is restarting the unit into the outer (§7.2): leave it the splash. The card
    /// and the listener are in the fd store already; closing them here changes nothing on screen.
    fn hand_off(&mut self) {
        info!("handing the splash over");
        self.let_go();
        if under_systemd() {
            let stored = self
                .state()
                .to_memfd()
                .and_then(|fd| handoff::store(handoff::FD_SPLASH, fd.as_fd()))
                .and_then(|()| handoff::barrier(BARRIER_TIMEOUT));
            if let Err(err) = stored {
                warn!("could not leave the splash in the fd store: {err}");
            }
        }
        let _ = fs::remove_file(MARKER);
    }

    /// The real root's `--hand-over` asks for what the fd store would hold (§7.6): the card,
    /// the listener and the splash, sent over `stream` once this process has let go of the rest.
    fn hand_over(&mut self, stream: &UnixStream) -> io::Result<()> {
        info!("handing the splash over");
        let state = self.state().to_memfd()?;
        // First: the outer writes the pid file again once it answers.
        self.let_go();
        let listener = self.server.listener_fd();
        let mut fds = vec![(handoff::FD_SPLASH, state.as_fd())];
        if let Some(fd) = listener {
            fds.push((handoff::FD_PLYMOUTH, fd));
        }
        if let Some(card) = &self.card {
            fds.push((handoff::FD_DRM, card.as_fd()));
        }
        handoff::send_fds(stream.as_fd(), &fds)
    }

    /// What goes with the splash's process: the VT keyboard back on (a prompt up now dies with
    /// its connection, §7.2) and the pid file, which the outer writes again once it answers.
    fn let_go(&mut self) {
        self.keyboard = None;
        self.entry.clear();
        self.server.release_pid_file();
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
            // The framebuffer and the mapping each hold the buffer; the handle would only keep it
            // alive on the card's file, which outlives us in the fd store, past the outer's first
            // flip: a whole screen's worth per buffer, every boot.
            card.close_buffer(drm::buffer::Buffer::handle(&db))?;
            Ok(Buf {
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
        frame: &Frame,
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
            paint::paint(&mut canvas, frame, theme);
        } else {
            paint::paint_spinner(&mut canvas, &frame.items, theme);
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

/// A socket on the kernel's uevents (`NETLINK_KOBJECT_UEVENT`, its multicast group): no udev
/// needed, which the initramfs stops before the switch.
fn open_uevents() -> io::Result<OwnedFd> {
    // SAFETY: plain syscalls; the fd is owned at once.
    let raw = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created, and ours.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: an all-zero sockaddr_nl is valid.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = 1;
    // SAFETY: the address above, its size beside it.
    let r = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const addr).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// Read every uevent waiting on `fd`: whether one was a DRM hotplug.
fn drm_hotplug(fd: BorrowedFd<'_>) -> bool {
    let mut buf = [0u8; 8192];
    let mut hotplug = false;
    loop {
        // SAFETY: a buffer of ours, its length beside it.
        let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        let Ok(n) = usize::try_from(n) else {
            return hotplug;
        };
        let mut fields = buf[..n].split(|&b| b == 0);
        hotplug |=
            fields.clone().any(|f| f == b"SUBSYSTEM=drm") && fields.any(|f| f == b"HOTPLUG=1");
    }
}

fn open_keyboard() -> Option<Keyboard> {
    keyboard::open_system()
        .inspect_err(|err| warn!("no keyboard for passwords: {err}"))
        .ok()
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
