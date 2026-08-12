// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A minimal `xdg_session_management_v1` client, for driving session restore against a *live*
//! compositor.
//!
//! The conformance corpus in `src/tests/gnome.rs` drives this protocol through the headless
//! fixture, which is the right place for the rules. What it cannot tell you is whether the seat
//! you are looking at behaves the same way — a rebuilt binary does not reach a running session
//! until it is restarted, and a bug reported from the seat has to be reproduced *there* before it
//! can be believed. This is that client.
//!
//! Two runs make a test:
//!
//! ```text
//! # First run: prints the session id, holds the windows open until killed.
//! session-probe --windows 2
//! # ... move a window somewhere with `synoik msg action ...`, then Ctrl-C (or SIGTERM):
//! # the toplevels are destroyed on the way out, which is what makes the compositor save.
//!
//! # Second run: ask for the same windows back.
//! session-probe --session-id <ID> --restore --windows 2
//! ```
//!
//! Everything it learns goes to stdout, one line per event, so a run is diffable against the
//! previous one.

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use rustix::time::Timespec;
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_shm::{Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};

/// The client half of the session protocol, generated from the same XML the compositor serves.
///
/// wayland-protocols 0.32 ships this XML but no bindings module, which is also why synoik vendors
/// the file — see `src/protocols/raw.rs`.
mod session {
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
    #![allow(missing_docs, clippy::all)]

    use wayland_client;
    use wayland_client::backend as wayland_backend;
    use wayland_client::protocol::*;
    use wayland_protocols::xdg::shell::client::xdg_toplevel;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::xdg::shell::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("../../resources/xdg-session-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("../../resources/xdg-session-management-v1.xml");
}

use session::xdg_session_manager_v1::{Reason, XdgSessionManagerV1};
use session::xdg_session_v1::{self, XdgSessionV1};
use session::xdg_toplevel_session_v1::{self, XdgToplevelSessionV1};

const HELP: &str = "\
session-probe — drive xdg_session_management_v1 against a live compositor

    --windows N        how many toplevels to create (default 2)
    --names a,b        names to register them under (default win0,win1,...)
    --session-id ID    join an existing session instead of creating one
    --restore          ask for the toplevels back (restore_toplevel) rather than
                       merely registering them (add_toplevel)
    --reason R         launch | recover | session-restore (default launch)
    --hold SECS        exit after SECS instead of waiting for a signal
    --quit-on-configure  exit as soon as every window has mapped; useful for a
                       one-shot 'where did they land' check
    --retitle-after-configure
                       set_title in the gap between the initial configure and
                       the first commit, the way an app that names a window
                       after its own content does. That recompute used to wipe
                       the seeds a restore had just written, so a restored
                       window came back in the wrong place; without this flag
                       the probe titles before its first commit and never
                       reaches the seam.
    --quit-style S     how to tear down on the way out (default all-windows):
                         all-windows  destroy every toplevel, then the session
                         session-first destroy the session, then the toplevels
                         session-mid  destroy one toplevel, then the session,
                                      then the rest
                       Which of these loses a window's saved state is exactly
                       the question when an app comes back on the wrong desktop.
    -h, --help         this

Exiting destroys the toplevels, which is what makes the compositor save the
session — killing the probe with SIGKILL saves nothing.
";

struct Opts {
    windows: usize,
    names: Option<Vec<String>>,
    session_id: Option<String>,
    restore: bool,
    reason: Reason,
    hold: Option<Duration>,
    quit_on_configure: bool,
    quit_style: QuitStyle,
    retitle_after_configure: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QuitStyle {
    AllWindows,
    SessionFirst,
    SessionMid,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            windows: 2,
            names: None,
            session_id: None,
            restore: false,
            reason: Reason::Launch,
            hold: None,
            quit_on_configure: false,
            quit_style: QuitStyle::AllWindows,
            retitle_after_configure: false,
        }
    }
}

fn parse_args() -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--windows" => {
                opts.windows = value()?
                    .parse()
                    .map_err(|_| String::from("--windows wants a number"))?
            }
            "--names" => {
                opts.names = Some(value()?.split(',').map(String::from).collect());
            }
            "--session-id" => opts.session_id = Some(value()?),
            "--restore" => opts.restore = true,
            "--reason" => {
                opts.reason = match value()?.as_str() {
                    "launch" => Reason::Launch,
                    "recover" => Reason::Recover,
                    "session-restore" | "session_restore" => Reason::SessionRestore,
                    other => return Err(format!("unknown reason {other}")),
                }
            }
            "--hold" => {
                let secs: u64 = value()?
                    .parse()
                    .map_err(|_| String::from("--hold wants a number of seconds"))?;
                opts.hold = Some(Duration::from_secs(secs));
            }
            "--quit-on-configure" => opts.quit_on_configure = true,
            "--retitle-after-configure" => opts.retitle_after_configure = true,
            "--quit-style" => {
                opts.quit_style = match value()?.as_str() {
                    "all-windows" => QuitStyle::AllWindows,
                    "session-first" => QuitStyle::SessionFirst,
                    "session-mid" => QuitStyle::SessionMid,
                    other => return Err(format!("unknown quit style {other}")),
                }
            }
            other => return Err(format!("unexpected argument {other}")),
        }
    }
    if let Some(names) = &opts.names {
        opts.windows = names.len();
    }
    Ok(opts)
}

/// One toplevel and what we have learned about it.
struct Window {
    name: String,
    surface: WlSurface,
    xdg_surface: XdgSurface,
    toplevel: XdgToplevel,
    handle: Option<XdgToplevelSessionV1>,
    /// The size the compositor asked for, and the states it sent with it.
    configured: Option<(i32, i32)>,
    states: Vec<xdg_toplevel::State>,
    pending_ack: Option<u32>,
    mapped: bool,
    /// Whether `xdg_toplevel_session_v1.restored` arrived, and whether it arrived *before* the
    /// first configure — the spec pins that ordering and it is worth checking on a live seat.
    restored: bool,
    restored_before_configure: Option<bool>,
}

struct State {
    opts: Opts,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    manager: Option<XdgSessionManagerV1>,
    session_id: Option<String>,
    windows: Vec<Window>,
    closed: bool,
}

impl State {
    fn window_mut(&mut self, surface: &XdgSurface) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| &w.xdg_surface == surface)
    }
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("session-probe: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    let conn = Connection::connect_to_env().expect("connect to the Wayland display");
    let (globals, mut queue) = wayland_client::globals::registry_queue_init::<State>(&conn)
        .expect("initial registry roundtrip");
    let qh = queue.handle();

    let mut state = State {
        opts,
        compositor: None,
        shm: None,
        wm_base: None,
        manager: None,
        session_id: None,
        windows: Vec::new(),
        closed: false,
    };

    for global in globals.contents().clone_list() {
        match global.interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(globals.registry().bind(
                    global.name,
                    4.min(global.version),
                    &qh,
                    (),
                ))
            }
            "wl_shm" => state.shm = Some(globals.registry().bind(global.name, 1, &qh, ())),
            "xdg_wm_base" => state.wm_base = Some(globals.registry().bind(global.name, 1, &qh, ())),
            "xdg_session_manager_v1" => {
                state.manager = Some(globals.registry().bind(global.name, 1, &qh, ()))
            }
            _ => {}
        }
    }

    let compositor = state.compositor.clone().expect("no wl_compositor");
    let wm_base = state.wm_base.clone().expect("no xdg_wm_base");
    let shm = state.shm.clone().expect("no wl_shm");
    let Some(manager) = state.manager.clone() else {
        eprintln!("session-probe: this compositor does not advertise xdg_session_manager_v1");
        std::process::exit(1);
    };

    let session = manager.get_session(
        state.opts.reason,
        state.opts.session_id.clone(),
        &qh,
        (),
    );
    // The id only comes back on `created`; `restored` means the compositor already had it.
    queue.roundtrip(&mut state).expect("roundtrip after get_session");
    println!(
        "session: id={} reason={:?} asked-for={:?}",
        state.session_id.as_deref().unwrap_or("<none yet>"),
        state.opts.reason,
        state.opts.session_id,
    );

    let names: Vec<String> = match &state.opts.names {
        Some(names) => names.clone(),
        None => (0..state.opts.windows)
            .map(|i| format!("win{i}"))
            .collect(),
    };

    for name in &names {
        let surface = compositor.create_surface(&qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(format!("session-probe {name}"));
        toplevel.set_app_id("session-probe".into());

        // Register (or ask for) the toplevel *before* the first commit: `restore_toplevel` on a
        // surface that already had its initial commit is `already_mapped`.
        let handle = if state.opts.restore {
            Some(session.restore_toplevel(&toplevel, name.clone(), &qh, name.clone()))
        } else {
            session.add_toplevel(&toplevel, name.clone(), &qh, name.clone());
            None
        };

        surface.commit();
        state.windows.push(Window {
            name: name.clone(),
            surface,
            xdg_surface,
            toplevel,
            handle,
            configured: None,
            states: Vec::new(),
            pending_ack: None,
            mapped: false,
            restored: false,
            restored_before_configure: None,
        });
    }

    let mut pool = ShmPool::new();
    let started = Instant::now();
    while !state.closed {
        // Read with a timeout rather than `blocking_dispatch`. A blocking dispatch only returns
        // when the compositor says something, so every *time-based* exit — `--hold`, and any
        // signal handler — would only be noticed on the next unrelated event. A probe whose
        // `--hold 30` silently means "until the compositor happens to speak" is worse than one
        // without the flag: the first run of this tool sat there for six minutes.
        queue.flush().expect("flush");
        if let Some(guard) = conn.prepare_read() {
            let fd = conn.as_fd();
            let mut fds = [PollFd::new(&fd, PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&Timespec { tv_sec: 0, tv_nsec: 100_000_000 }))
            {
                Ok(n) if n > 0 => {
                    let _ = guard.read();
                }
                _ => drop(guard),
            }
        }
        queue.dispatch_pending(&mut state).expect("dispatch");

        // Map anything that has been configured and hasn't drawn yet.
        for i in 0..state.windows.len() {
            let (size, needs_map) = {
                let w = &state.windows[i];
                (w.configured, !w.mapped && w.configured.is_some())
            };
            if !needs_map {
                continue;
            }
            let (cw, ch) = size.unwrap();
            let (cw, ch) = (if cw > 0 { cw } else { 400 }, if ch > 0 { ch } else { 300 });
            let Some(buffer) = pool.try_buffer(&shm, &qh, cw, ch) else {
                continue;
            };
            let w = &mut state.windows[i];
            if let Some(serial) = w.pending_ack.take() {
                w.xdg_surface.ack_configure(serial);
            }
            // A client naming the window after its own content, in the gap between the initial
            // configure and the first commit. This is the shape that broke session restore in
            // `6e4fb6d9`: the compositor recomputes the window rules from scratch on `set_title`
            // and used to assign over the seeds restore had just written, so the window came back
            // the right size on the right desktop in the wrong place. Not a terminal-only habit --
            // any app that titles itself from the session/tab it is about does this, and ghost
            // does it unconditionally on every window open.
            if state.opts.retitle_after_configure {
                w.toplevel.set_title(format!("{} configured", w.name));
                println!("retitled: name={} (post-configure, pre-attach)", w.name);
            }
            w.surface.attach(Some(&buffer), 0, 0);
            w.surface.damage_buffer(0, 0, cw, ch);
            w.surface.commit();
            w.mapped = true;
            println!(
                "mapped: name={} size={}x{} states={:?} restored={}",
                w.name, cw, ch, w.states, w.restored
            );
        }

        if state.opts.quit_on_configure && state.windows.iter().all(|w| w.mapped) {
            break;
        }
        if let Some(hold) = state.opts.hold {
            if started.elapsed() >= hold {
                break;
            }
        }
    }

    // Unmapping is what makes the compositor save. *When* the session object dies relative to the
    // toplevels is the interesting variable: a session destroyed first may take the registrations
    // with it, and then the unmap that follows has nothing to look itself up under.
    println!(
        "quitting: style={:?}, destroying {} toplevel(s)",
        state.opts.quit_style,
        state.windows.len()
    );
    let destroy = |w: &Window| {
        if let Some(handle) = &w.handle {
            handle.destroy();
        }
        w.toplevel.destroy();
        w.xdg_surface.destroy();
        w.surface.destroy();
    };
    match state.opts.quit_style {
        QuitStyle::AllWindows => {
            state.windows.iter().for_each(destroy);
            session.destroy();
        }
        QuitStyle::SessionFirst => {
            session.destroy();
            let _ = queue.roundtrip(&mut state);
            state.windows.iter().for_each(destroy);
        }
        QuitStyle::SessionMid => {
            if let Some(first) = state.windows.first() {
                destroy(first);
            }
            let _ = queue.roundtrip(&mut state);
            session.destroy();
            let _ = queue.roundtrip(&mut state);
            state.windows.iter().skip(1).for_each(destroy);
        }
    }
    // A plain flush can return before the compositor has processed the destroys; a roundtrip
    // guarantees it has, which is the difference between saving the session and not.
    let _ = queue.roundtrip(&mut state);
    println!(
        "done: session id={}",
        state.session_id.as_deref().unwrap_or("<none>")
    );
}

// -- shm ----------------------------------------------------------------------------------------

const SLOTS: usize = 4;

type Busy = Arc<AtomicBool>;

/// A tiny rotating shm pool. Same shape as blur-probe's, minus the sizing games: this probe only
/// ever paints a flat fill, because what it measures is *where* a window lands, never how it looks.
struct ShmPool {
    _file: Option<std::fs::File>,
    pool: Option<WlShmPool>,
    stride: usize,
    map: Option<*mut u8>,
    busy: [Busy; SLOTS],
    next: usize,
}

impl ShmPool {
    fn new() -> Self {
        Self {
            _file: None,
            pool: None,
            stride: 0,
            map: None,
            busy: std::array::from_fn(|_| Busy::default()),
            next: 0,
        }
    }

    fn try_buffer(
        &mut self,
        shm: &WlShm,
        qh: &QueueHandle<State>,
        w: i32,
        h: i32,
    ) -> Option<WlBuffer> {
        use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
        use rustix::mm::{mmap, MapFlags, ProtFlags};

        let needed = (w as usize) * (h as usize) * 4;
        if needed > self.stride {
            let stride = (needed * 2).next_power_of_two();
            let capacity = stride * SLOTS;
            let fd = memfd_create("session-probe", MemfdFlags::CLOEXEC).expect("memfd_create");
            ftruncate(&fd, capacity as u64).expect("ftruncate");
            let file = std::fs::File::from(fd);
            let map = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    capacity,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    file.as_fd(),
                    0,
                )
                .expect("mmap")
            };
            if let Some(pool) = self.pool.take() {
                pool.destroy();
            }
            self.pool = Some(shm.create_pool(file.as_fd(), capacity as i32, qh, ()));
            self._file = Some(file);
            self.map = Some(map as *mut u8);
            self.stride = stride;
            self.busy = std::array::from_fn(|_| Busy::default());
        }

        let slot = (0..SLOTS)
            .map(|i| (self.next + i) % SLOTS)
            .find(|&i| !self.busy[i].load(Ordering::Relaxed))?;
        self.next = (slot + 1) % SLOTS;

        let offset = slot * self.stride;
        let map = self.map.expect("mapped pool");
        let pixels = unsafe { std::slice::from_raw_parts_mut(map.add(offset), needed) };
        for chunk in pixels.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0x40, 0x20, 0x80, 0xff]);
        }

        self.busy[slot].store(true, Ordering::Relaxed);
        Some(self.pool.as_ref().unwrap().create_buffer(
            offset as i32,
            w,
            h,
            w * 4,
            Format::Argb8888,
            qh,
            self.busy[slot].clone(),
        ))
    }
}

// -- dispatch -----------------------------------------------------------------------------------

impl Dispatch<WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            if let Some(window) = state.window_mut(surface) {
                window.pending_ack = Some(serial);
                if window.configured.is_none() {
                    window.configured = Some(window.configured.unwrap_or((0, 0)));
                }
                // The ordering the spec pins: `restored` must precede the first configure.
                if window.restored_before_configure.is_none() {
                    window.restored_before_configure = Some(window.restored);
                }
            }
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        toplevel: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(window) = state
            .windows
            .iter_mut()
            .find(|w| &w.toplevel == toplevel)
        else {
            return;
        };
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                window.configured = Some((width, height));
                window.states = states
                    .chunks_exact(4)
                    .filter_map(|c| {
                        let raw = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                        xdg_toplevel::State::try_from(raw).ok()
                    })
                    .collect();
            }
            xdg_toplevel::Event::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<XdgSessionManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &XdgSessionManagerV1,
        _: session::xdg_session_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgSessionV1,
        event: xdg_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_session_v1::Event::Created { session_id: id } => {
                println!("event: created id={id}");
                state.session_id = Some(id);
            }
            xdg_session_v1::Event::Restored => {
                println!("event: restored (the compositor already knew this id)");
            }
            xdg_session_v1::Event::Replaced => println!("event: replaced"),
        }
    }
}

impl Dispatch<XdgToplevelSessionV1, String> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevelSessionV1,
        event: xdg_toplevel_session_v1::Event,
        name: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let xdg_toplevel_session_v1::Event::Restored = event;
        println!("event: toplevel restored name={name}");
        if let Some(window) = state.windows.iter_mut().find(|w| &w.name == name) {
            window.restored = true;
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);

impl Dispatch<WlBuffer, Busy> for State {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        busy: &Busy,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Relaxed);
            buffer.destroy();
        }
    }
}
