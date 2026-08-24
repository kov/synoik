// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The X11 ↔ Wayland selection bridge: clipboard and primary, both ways.
//!
//! **Why the compositor owns this.** xwayland-satellite bridges selections too, but only over
//! `wl_data_device` / `zwp_primary_selection`, and both are focus-gated: it sets the Wayland
//! selection only once one of its X windows has held keyboard focus (`server/selection.rs:124`
//! guards on `last_kb_serial`), and the compositor sends it `selection` events only while one
//! does. An X client with no window — spice-vdagent, which is how a VM shares the host clipboard —
//! therefore never reaches the Wayland clipboard in either direction.
//!
//! GNOME has no such gate because Xwayland is *inside* mutter: `src/x11/meta-x11-selection.c`
//! watches the X selections as a plain X client and hands ownership to the compositor-internal
//! `MetaSelection` registry, so neither focus nor a serial is involved. This module is that,
//! for our out-of-process Xwayland: we connect to satellite's display as an ordinary X client,
//! own CLIPBOARD/PRIMARY on behalf of Wayland, and publish the X side through
//! `set_data_device_selection` / `set_primary_selection`, which answer to nobody's focus.
//!
//! Two bridges cannot coexist — each would re-export every selection back the way it came, and
//! the second export cancels the first source — so satellite is barred from selections entirely
//! with `set_selection_excluded_client` (its drag-and-drop is untouched).
//!
//! **Shape.** All X protocol lives on one thread; the compositor never blocks on a round trip to
//! Xwayland. The seam between them is plain data — [`FromX`] and [`ToX`] carry mime strings,
//! byte buffers and pipe fds, never X or Wayland objects — so the X side can be moved out of
//! process later without touching the compositor side.
//!
//! **Lazy, like mutter.** Taking the X selection publishes only the *mime list*; the bytes are
//! fetched when someone actually pastes. Eagerly converting would make every host-side copy pull
//! its payload across (an image, at vdagent's leisure) for a paste that may never come.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::os::fd::{AsFd as _, OwnedFd};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use calloop::RegistrationToken;
use smithay::reexports::rustix::fs::{fcntl_setfl, OFlags};
use smithay::reexports::rustix::io::{retry_on_intr, write, Errno};
use smithay::reexports::rustix::pipe::{pipe_with, PipeFlags};
use smithay::wayland::selection::SelectionTarget;
use x11rb::connection::Connection as _;
use x11rb::protocol::xfixes::{
    ConnectionExt as _, SelectionEventMask, SelectionNotifyEvent as XfixesSelectionNotifyEvent,
};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property,
    PropertyNotifyEvent, SelectionClearEvent, SelectionNotifyEvent, SelectionRequestEvent, Window,
    WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{atom_manager, COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

/// The most one transfer may carry, in bytes.
///
/// Neither side of a paste is trusted to be small — an X client can hand us a video frame, and a
/// Wayland client can hand back whatever it likes — and every transfer is buffered whole so that
/// the X thread never blocks on a pipe. This is the ceiling that keeps a hostile or merely
/// enthusiastic clipboard from being an OOM. Well above any real clipboard image.
const TRANSFER_LIMIT: usize = 64 * 1024 * 1024;

/// How long a peer has to finish a transfer before we abandon it.
///
/// A wedged clipboard owner on either side would otherwise hold a helper thread, a pipe and a
/// pending X request for the life of the session.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to keep trying to reach satellite's X display before giving up.
///
/// We connect from the spawn path, so Xwayland is starting as we dial: the first few attempts are
/// expected to fail. Satellite itself is the thing that would not come up at all, and it logs
/// that; the bridge just stops trying.
const CONNECT_ATTEMPTS: u32 = 50;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(100);

atom_manager! {
    /// Every atom the bridge interns up front, so no code path needs a round trip to name one.
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        PRIMARY,
        TARGETS,
        TIMESTAMP,
        MULTIPLE,
        INCR,
        UTF8_STRING,
        STRING,
        TEXT,
        COMPOUND_TEXT,
        // Where a payload transfer lands on our own window, and -- separately, because a target
        // query can arrive while a paste is in flight -- where a TARGETS reply lands.
        _SYNOIK_SELECTION,
        _SYNOIK_TARGETS,
    }
}

/// What the X side tells the compositor.
#[derive(Debug)]
pub enum FromX {
    /// An X client owns `target` and offers these mime types. Bytes come later, on demand.
    Offer {
        target: SelectionTarget,
        mime_types: Vec<String>,
    },
    /// Nobody owns `target` on the X side any more.
    Cleared { target: SelectionTarget },
    /// An X client is pasting: it wants `mime_type` of the Wayland-owned `target`.
    ///
    /// Answer with [`ToX::Data`] carrying the same `id`, always — a request left unanswered is an
    /// X client blocked forever waiting for its `SelectionNotify`.
    Request {
        target: SelectionTarget,
        id: u64,
        mime_type: String,
    },
}

/// What the compositor tells the X side.
enum ToX {
    /// A Wayland client owns `target`; take it on X too, offering these mime types.
    Offer {
        target: SelectionTarget,
        mime_types: Vec<String>,
    },
    /// Nothing owns `target` on the Wayland side; drop X ownership if we hold it.
    Cleared { target: SelectionTarget },
    /// The answer to [`FromX::Request`] `id`: the read end of a pipe the owner is filling, or
    /// `None` if there was nothing to give.
    Data { id: u64, fd: Option<OwnedFd> },
    /// A Wayland client is pasting: write `mime_type` of the X-owned `target` into `fd`.
    Fetch {
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    },
}

/// The compositor's handle on the bridge.
///
/// Dropping it drops the channel, which is how the X thread learns to exit.
pub struct Bridge {
    to_x: Sender<ToX>,
    /// Written to after every send: the X thread is parked in `poll`, not on the channel.
    wake: OwnedFd,
    /// The event source carrying [`FromX`], so replacing a bridge does not leak it.
    pub token: Option<RegistrationToken>,
}

impl Bridge {
    fn send(&self, msg: ToX) {
        if self.to_x.send(msg).is_err() {
            return;
        }
        // A full pipe means the X thread has not drained its wakeups yet, which is exactly the
        // case where it does not need another one.
        let _ = retry_on_intr(|| write(&self.wake, &[0]));
    }

    /// A Wayland client (or the shell itself) took `target`; mirror that onto X.
    pub fn offer(&self, target: SelectionTarget, mime_types: Vec<String>) {
        self.send(ToX::Offer { target, mime_types });
    }

    /// Nothing owns `target` on the Wayland side any more.
    pub fn clear(&self, target: SelectionTarget) {
        self.send(ToX::Cleared { target });
    }

    /// Ask the X owner of `target` for `mime_type`, written into `fd`.
    pub fn fetch(&self, target: SelectionTarget, mime_type: String, fd: OwnedFd) {
        self.send(ToX::Fetch {
            target,
            mime_type,
            fd,
        });
    }

    /// Answer a [`FromX::Request`].
    pub fn answer(&self, id: u64, fd: Option<OwnedFd>) {
        self.send(ToX::Data { id, fd });
    }
}

/// Start the bridge against `display_name` (satellite's `:N`).
///
/// Returns the compositor-side handle and the receiver to install on the event loop. The X thread
/// dials the display in a retry loop, so this is safe to call the moment satellite is spawned.
pub fn start(display_name: String) -> anyhow::Result<(Bridge, calloop::channel::Channel<FromX>)> {
    let (to_main, from_x) = calloop::channel::channel();
    let (to_x, x_rx) = std::sync::mpsc::channel();
    let (wake_read, wake_write) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)?;
    // The helper threads that read a Wayland client's pipe wake the X thread the same way the
    // compositor does, so they need a write end of their own.
    let helper_wake = wake_write.try_clone()?;

    thread::Builder::new()
        .name("X11 selection".to_owned())
        .spawn(move || run(&display_name, to_main, x_rx, wake_read, helper_wake))?;

    Ok((
        Bridge {
            to_x,
            wake: wake_write,
            token: None,
        },
        from_x,
    ))
}

// ---------------------------------------------------------------------------------------------
// mime ↔ atom
// ---------------------------------------------------------------------------------------------

/// The atom an X client expects for `mime`, or `None` when the name cannot be an X target.
///
/// X predates mime types, so the two text targets have names of their own; everything else is
/// carried under an atom whose *name is the mime type*, which is what every toolkit does and what
/// mutter's `meta-x11-selection.c` assumes.
fn atom_for_mime(atoms: &Atoms, mime: &str) -> Option<Atom> {
    match mime {
        "text/plain;charset=utf-8" | "UTF8_STRING" => Some(atoms.UTF8_STRING),
        "text/plain" => Some(atoms.STRING),
        _ => None,
    }
}

/// The mime types an X target stands for, most specific first.
///
/// `TEXT` and `COMPOUND_TEXT` are deliberately not mapped: their encoding is locale-dependent, so
/// a Wayland client told "text/plain" would be handed bytes it cannot interpret. We *offer* them
/// (an X client asking for `TEXT` gets utf-8, which is what everyone means by it now) but we never
/// import through them.
fn mimes_for_atom(atoms: &Atoms, atom: Atom, name: &str) -> Vec<String> {
    if atom == atoms.UTF8_STRING {
        return vec!["text/plain;charset=utf-8".to_owned()];
    }
    if atom == atoms.STRING {
        return vec!["text/plain".to_owned()];
    }
    // An atom whose name has no slash is an X-ism (TARGETS, TIMESTAMP, MULTIPLE, SAVE_TARGETS,
    // a toolkit's private target...), not a mime type. Offering it would put junk on the
    // Wayland clipboard that no client can ask for.
    if name.contains('/') {
        return vec![name.to_owned()];
    }
    Vec::new()
}

/// The full target list to advertise for a Wayland selection offering `mime_types`.
fn targets_for_mimes(
    atoms: &Atoms,
    mime_types: &[String],
    extra: &HashMap<String, Atom>,
) -> Vec<Atom> {
    let mut targets = vec![atoms.TARGETS, atoms.TIMESTAMP];

    for mime in mime_types {
        if let Some(atom) = atom_for_mime(atoms, mime) {
            if !targets.contains(&atom) {
                targets.push(atom);
            }
        } else if let Some(&atom) = extra.get(mime) {
            if !targets.contains(&atom) {
                targets.push(atom);
            }
        }
    }

    // utf-8 text also answers to the two legacy text targets; an X client that asks for either
    // gets utf-8 bytes, which is what they mean in practice.
    if targets.contains(&atoms.UTF8_STRING) {
        for legacy in [atoms.TEXT, atoms.COMPOUND_TEXT] {
            if !targets.contains(&legacy) {
                targets.push(legacy);
            }
        }
    }

    targets
}

// ---------------------------------------------------------------------------------------------
// the X thread
// ---------------------------------------------------------------------------------------------

/// What we hold on X on behalf of a Wayland selection owner.
struct Owned {
    mime_types: Vec<String>,
    targets: Vec<Atom>,
    /// Atom per non-text mime type, interned when we took ownership.
    mime_atoms: HashMap<String, Atom>,
    /// What to answer a `TIMESTAMP` request with.
    since: u32,
}

/// An X client waiting on Wayland data.
struct Serving {
    requestor: Window,
    selection: Atom,
    target: Atom,
    property: Atom,
    time: u32,
}

/// A Wayland client waiting on X data.
struct Fetching {
    fd: OwnedFd,
    /// Set once the owner answered with an `INCR` property and we are collecting chunks.
    incr: bool,
    buf: Vec<u8>,
    /// When to give up on the X owner. Without it a selection owner that answers nothing wedges
    /// the queue and hangs the pasting client for the life of the session.
    deadline: Instant,
}

struct XState {
    conn: RustConnection,
    window: Window,
    atoms: Atoms,
    /// Selections a Wayland client owns, that we hold on X for it.
    owned: HashMap<Atom, Owned>,
    /// Requests from X clients we have forwarded to the compositor, by id.
    serving: HashMap<u64, Serving>,
    next_serving_id: u64,
    /// The in-flight X→Wayland conversion, if any. One at a time: they all land in the same
    /// property on the same window, so overlapping them would interleave two payloads.
    fetching: Option<Fetching>,
    /// Fetches waiting for the one in flight, oldest first.
    fetch_queue: VecDeque<(SelectionTarget, String, OwnedFd)>,
    to_main: calloop::channel::Sender<FromX>,
    /// Bytes a helper thread read out of a Wayland client's pipe, by serving id.
    from_helpers: Receiver<(u64, Option<Vec<u8>>)>,
    to_helpers: Sender<(u64, Option<Vec<u8>>)>,
    /// A write end of the wake pipe, for the helper threads.
    helper_wake: Arc<OwnedFd>,
    /// The most recent server timestamp we have seen.
    ///
    /// ICCCM wants a real timestamp for `SetSelectionOwner`, never `CurrentTime`: with
    /// `CurrentTime` two clients racing for the selection cannot be ordered. Any timestamp the
    /// server has given us is a valid one to use; we only fall back before the first event.
    last_time: u32,
}

fn run(
    display_name: &str,
    to_main: calloop::channel::Sender<FromX>,
    rx: Receiver<ToX>,
    wake: OwnedFd,
    helper_wake: OwnedFd,
) {
    let Some((conn, screen_num)) = connect(display_name) else {
        warn!("X11 selection bridge: could not reach {display_name}, clipboard will not bridge");
        return;
    };

    let mut state = match XState::new(conn, screen_num, to_main, helper_wake) {
        Ok(state) => state,
        Err(err) => {
            warn!("X11 selection bridge: setup failed: {err}");
            return;
        }
    };

    debug!("X11 selection bridge up on {display_name}");

    if let Err(err) = state.event_loop(&rx, &wake) {
        warn!("X11 selection bridge: {err}");
    }
}

fn connect(display_name: &str) -> Option<(RustConnection, usize)> {
    for attempt in 0..CONNECT_ATTEMPTS {
        match RustConnection::connect(Some(display_name)) {
            Ok(conn) => return Some(conn),
            Err(err) => {
                if attempt + 1 == CONNECT_ATTEMPTS {
                    warn!("X11 selection bridge: connect failed: {err}");
                }
                thread::sleep(CONNECT_RETRY_DELAY);
            }
        }
    }
    None
}

/// Ask the server what time it is, the only way X offers: make it stamp an event.
fn seed_server_time(conn: &RustConnection, window: Window, prop: Atom) -> anyhow::Result<u32> {
    conn.change_property8(PropMode::APPEND, window, prop, AtomEnum::STRING, &[])?;
    conn.flush()?;
    loop {
        if let Event::PropertyNotify(event) = conn.wait_for_event()? {
            if event.window == window && event.atom == prop {
                conn.delete_property(window, prop)?;
                return Ok(event.time);
            }
        }
    }
}

/// Log what one message or event went wrong with, and carry on.
///
/// A malformed request from one X client, or a window that died mid-transfer, is that client's
/// problem: it must not take the clipboard down with it. Only the connection itself failing ends
/// the thread, and that is handled where the connection is read.
fn report(res: anyhow::Result<()>) {
    if let Err(err) = res {
        debug!("X11 selection bridge: {err}");
    }
}

impl XState {
    fn new(
        conn: RustConnection,
        screen_num: usize,
        to_main: calloop::channel::Sender<FromX>,
        helper_wake: OwnedFd,
    ) -> anyhow::Result<Self> {
        let atoms = Atoms::new(&conn)?.reply()?;
        let root = conn.setup().roots[screen_num].root;

        let window = conn.generate_id()?;
        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            // PropertyNotify is how an INCR transfer arrives; the window is never mapped, so it
            // costs nothing else.
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;

        conn.xfixes_query_version(5, 0)?.reply()?;
        for selection in [atoms.CLIPBOARD, atoms.PRIMARY] {
            conn.xfixes_select_selection_input(
                window,
                selection,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )?;
        }
        conn.flush()?;

        // ICCCM wants a real timestamp for `SetSelectionOwner`, and the only way to ask the
        // server for one is to make it stamp something: a zero-length append to a property of
        // our own, whose `PropertyNotify` carries the current server time. Blocking for it is
        // fine here -- the connection is ours alone and nothing else is in flight yet.
        let last_time =
            seed_server_time(&conn, window, atoms._SYNOIK_SELECTION).unwrap_or_else(|err| {
                debug!("X11 selection bridge: no server timestamp ({err}), using CurrentTime");
                CURRENT_TIME
            });

        let (to_helpers, from_helpers) = std::sync::mpsc::channel();

        Ok(Self {
            conn,
            window,
            atoms,
            owned: HashMap::new(),
            serving: HashMap::new(),
            next_serving_id: 0,
            fetching: None,
            fetch_queue: VecDeque::new(),
            to_main,
            from_helpers,
            to_helpers,
            helper_wake: Arc::new(helper_wake),
            last_time,
        })
    }

    fn target_of(&self, selection: Atom) -> Option<SelectionTarget> {
        if selection == self.atoms.CLIPBOARD {
            Some(SelectionTarget::Clipboard)
        } else if selection == self.atoms.PRIMARY {
            Some(SelectionTarget::Primary)
        } else {
            None
        }
    }

    fn atom_of(&self, target: SelectionTarget) -> Atom {
        match target {
            SelectionTarget::Clipboard => self.atoms.CLIPBOARD,
            SelectionTarget::Primary => self.atoms.PRIMARY,
        }
    }

    fn event_loop(&mut self, rx: &Receiver<ToX>, wake: &OwnedFd) -> anyhow::Result<()> {
        use smithay::reexports::rustix::event::{poll, PollFd, PollFlags, Timespec};

        loop {
            // Drain every source before parking: each can queue work while we serve another.
            loop {
                match rx.try_recv() {
                    Ok(msg) => report(self.handle_message(msg)),
                    Err(TryRecvError::Empty) => break,
                    // The compositor dropped the bridge: nothing left to serve.
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }

            while let Ok((id, data)) = self.from_helpers.try_recv() {
                report(self.answer_request(id, data));
            }

            // A connection error here is fatal -- satellite is gone, or the X server is -- so it
            // ends the thread, which closes the channel and tells the compositor.
            while let Some(event) = self.conn.poll_for_event()? {
                report(self.handle_event(event));
            }

            self.expire_fetch()?;
            self.conn.flush()?;

            let mut fds = [
                PollFd::from_borrowed_fd(self.conn.stream().as_fd(), PollFlags::IN),
                PollFd::from_borrowed_fd(wake.as_fd(), PollFlags::IN),
            ];

            // Only a fetch has a deadline; with nothing in flight there is nothing to wake for.
            let timeout = self.fetching.as_ref().map(|fetching| {
                let left = fetching.deadline.saturating_duration_since(Instant::now());
                Timespec {
                    tv_sec: left.as_secs() as _,
                    tv_nsec: left.subsec_nanos() as _,
                }
            });

            if let Err(err) = poll(&mut fds, timeout.as_ref()) {
                if err == Errno::INTR {
                    continue;
                }
                return Err(err.into());
            }

            if fds[1].revents().contains(PollFlags::IN) {
                let mut drain = [0u8; 64];
                while let Ok(n) =
                    retry_on_intr(|| smithay::reexports::rustix::io::read(wake.as_fd(), &mut drain))
                {
                    if n < drain.len() {
                        break;
                    }
                }
            }
        }
    }

    /// Give up on an X owner that never answered.
    fn expire_fetch(&mut self) -> anyhow::Result<()> {
        if self
            .fetching
            .as_ref()
            .is_some_and(|f| Instant::now() >= f.deadline)
        {
            debug!("X11 selection bridge: the X11 selection owner did not answer in time");
            if let Some(fetching) = self.fetching.as_mut() {
                fetching.buf.clear();
            }
            self.finish_fetch()?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // compositor → X
    // -----------------------------------------------------------------------------------------

    fn handle_message(&mut self, msg: ToX) -> anyhow::Result<()> {
        match msg {
            ToX::Offer { target, mime_types } => self.take_ownership(target, mime_types)?,
            ToX::Cleared { target } => self.drop_ownership(target)?,
            ToX::Data { id, fd } => self.read_for_request(id, fd),
            ToX::Fetch {
                target,
                mime_type,
                fd,
            } => self.start_fetch(target, mime_type, fd)?,
        }
        Ok(())
    }

    fn take_ownership(
        &mut self,
        target: SelectionTarget,
        mime_types: Vec<String>,
    ) -> anyhow::Result<()> {
        let selection = self.atom_of(target);

        // Every non-text mime type needs an atom of its own to appear in TARGETS.
        let mut mime_atoms = HashMap::new();
        for mime in &mime_types {
            if atom_for_mime(&self.atoms, mime).is_some() {
                continue;
            }
            if !mime.contains('/') {
                continue;
            }
            let atom = self.conn.intern_atom(false, mime.as_bytes())?.reply()?.atom;
            mime_atoms.insert(mime.clone(), atom);
        }

        let targets = targets_for_mimes(&self.atoms, &mime_types, &mime_atoms);

        let time = self.last_time;
        self.conn
            .set_selection_owner(self.window, selection, time)?;
        // Trust the server, not the request: another client can take it back between the two.
        let owner = self.conn.get_selection_owner(selection)?.reply()?.owner;
        if owner != self.window {
            debug!("X11 selection bridge: lost {selection} ownership immediately to {owner}");
            self.owned.remove(&selection);
            return Ok(());
        }

        self.owned.insert(
            selection,
            Owned {
                mime_types,
                targets,
                mime_atoms,
                since: time,
            },
        );
        Ok(())
    }

    fn drop_ownership(&mut self, target: SelectionTarget) -> anyhow::Result<()> {
        let selection = self.atom_of(target);
        if self.owned.remove(&selection).is_some() {
            self.conn
                .set_selection_owner(NONE, selection, self.last_time)?;
        }
        Ok(())
    }

    /// Start reading the Wayland owner's pipe for serving request `id`.
    ///
    /// On a helper thread, never here: the owner is another process, and a `read` that never
    /// returns would take every other X client's clipboard with it.
    fn read_for_request(&mut self, id: u64, fd: Option<OwnedFd>) {
        let Some(fd) = fd else {
            report(self.answer_request(id, None));
            return;
        };
        if !self.serving.contains_key(&id) {
            return;
        }

        let to_helpers = self.to_helpers.clone();
        let wake = self.helper_wake.clone();
        let res = thread::Builder::new()
            .name("X11 selection read".to_owned())
            .spawn(move || {
                let data = read_all(fd)
                    .inspect_err(|err| debug!("X11 selection bridge: reading a paste: {err}"))
                    .ok();
                let _ = to_helpers.send((id, data));
                let _ = retry_on_intr(|| write(&*wake, &[0]));
            });
        if res.is_err() {
            report(self.answer_request(id, None));
        }
    }

    /// Finish serving an X client: put the bytes on its property and notify it.
    fn answer_request(&mut self, id: u64, data: Option<Vec<u8>>) -> anyhow::Result<()> {
        let Some(serving) = self.serving.remove(&id) else {
            return Ok(());
        };

        let Some(data) = data else {
            return self.refuse(&serving);
        };

        // The property type is the target itself for a mime atom, and UTF8_STRING for the text
        // aliases: an X client that asked for TEXT expects to be told what it actually got.
        let ty = if serving.target == self.atoms.TEXT || serving.target == self.atoms.COMPOUND_TEXT
        {
            self.atoms.UTF8_STRING
        } else {
            serving.target
        };

        self.conn.change_property8(
            PropMode::REPLACE,
            serving.requestor,
            serving.property,
            ty,
            &data,
        )?;
        self.notify(&serving, serving.property)
    }

    /// Tell an X client we cannot answer: `property` is `None` per ICCCM.
    fn refuse(&self, serving: &Serving) -> anyhow::Result<()> {
        self.notify(serving, NONE)
    }

    fn notify(&self, serving: &Serving, property: Atom) -> anyhow::Result<()> {
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: serving.time,
            requestor: serving.requestor,
            selection: serving.selection,
            target: serving.target,
            property,
        };
        self.conn
            .send_event(false, serving.requestor, EventMask::NO_EVENT, event)?;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // X → compositor
    // -----------------------------------------------------------------------------------------

    fn start_fetch(
        &mut self,
        target: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) -> anyhow::Result<()> {
        if self.fetching.is_some() {
            self.fetch_queue.push_back((target, mime_type, fd));
            return Ok(());
        }

        let selection = self.atom_of(target);
        let atom = match atom_for_mime(&self.atoms, &mime_type) {
            Some(atom) => atom,
            None => {
                self.conn
                    .intern_atom(false, mime_type.as_bytes())?
                    .reply()?
                    .atom
            }
        };

        self.conn
            .delete_property(self.window, self.atoms._SYNOIK_SELECTION)?;
        self.conn.convert_selection(
            self.window,
            selection,
            atom,
            self.atoms._SYNOIK_SELECTION,
            self.last_time,
        )?;
        self.fetching = Some(Fetching {
            fd,
            incr: false,
            buf: Vec::new(),
            deadline: Instant::now() + TRANSFER_TIMEOUT,
        });
        Ok(())
    }

    fn finish_fetch(&mut self) -> anyhow::Result<()> {
        if let Some(fetching) = self.fetching.take() {
            // Hand the write off: a Wayland client that never reads must not wedge the X thread.
            let Fetching { fd, buf, .. } = fetching;
            thread::Builder::new()
                .name("X11 selection write".to_owned())
                .spawn(move || {
                    if let Err(err) = write_all(fd, &buf) {
                        debug!("X11 selection bridge: writing to a pasting client failed: {err}");
                    }
                })
                .ok();
        }

        if let Some((target, mime_type, fd)) = self.fetch_queue.pop_front() {
            self.start_fetch(target, mime_type, fd)?;
        }
        Ok(())
    }

    /// The atoms of a `TARGETS` reply.
    ///
    /// Read as 32-bit values rather than raw bytes: `value32` is what knows the server's byte
    /// order, and guessing native would be wrong the day this talks to a remote display.
    fn read_targets_property(&mut self) -> anyhow::Result<Vec<Atom>> {
        let reply = self
            .conn
            .get_property(
                true,
                self.window,
                self.atoms._SYNOIK_TARGETS,
                AtomEnum::ATOM,
                0,
                // A target list this long is not a target list.
                4096,
            )?
            .reply()?;
        Ok(reply.value32().map(|it| it.collect()).unwrap_or_default())
    }

    fn read_transfer_property(&mut self) -> anyhow::Result<(Atom, Vec<u8>)> {
        let mut out = Vec::new();
        let mut ty;
        let mut offset = 0u32;
        loop {
            let reply = self
                .conn
                .get_property(
                    false,
                    self.window,
                    self.atoms._SYNOIK_SELECTION,
                    AtomEnum::ANY,
                    offset,
                    // In 32-bit units, so 1 MiB a round.
                    256 * 1024,
                )?
                .reply()?;
            ty = reply.type_;
            let more = reply.bytes_after > 0;
            if out.len() + reply.value.len() > TRANSFER_LIMIT {
                warn!("X11 selection bridge: transfer over {TRANSFER_LIMIT} bytes, dropping it");
                out.clear();
                break;
            }
            offset += (reply.value.len() / 4) as u32;
            out.extend_from_slice(&reply.value);
            if !more {
                break;
            }
        }

        // Deleting is also what acknowledges a chunk, which is how an INCR transfer advances.
        self.conn
            .delete_property(self.window, self.atoms._SYNOIK_SELECTION)?;
        Ok((ty, out))
    }

    fn handle_event(&mut self, event: Event) -> anyhow::Result<()> {
        // Any timestamp the server has handed us is a valid one to use later; see `last_time`.
        match &event {
            Event::XfixesSelectionNotify(e) => self.last_time = e.timestamp,
            Event::SelectionNotify(e) => self.last_time = e.time,
            Event::SelectionRequest(e) if e.time != CURRENT_TIME => self.last_time = e.time,
            Event::SelectionClear(e) => self.last_time = e.time,
            Event::PropertyNotify(e) => self.last_time = e.time,
            _ => (),
        }

        match event {
            Event::XfixesSelectionNotify(event) => self.on_owner_change(event),
            Event::SelectionNotify(event) => self.on_selection_notify(event),
            Event::SelectionRequest(event) => self.on_selection_request(event),
            Event::SelectionClear(event) => self.on_selection_clear(event),
            Event::PropertyNotify(event) => self.on_property_notify(event),
            // A request we made against a window that died mid-transfer, most likely.
            Event::Error(err) => {
                debug!("X11 selection bridge: protocol error: {err:?}");
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// An X client took (or dropped) a selection: publish its target list to the compositor.
    fn on_owner_change(&mut self, event: XfixesSelectionNotifyEvent) -> anyhow::Result<()> {
        let Some(target) = self.target_of(event.selection) else {
            return Ok(());
        };
        // Our own ownership, echoed back. Ignoring it is what keeps the two sides from
        // ping-ponging a selection between them.
        if event.owner == self.window {
            return Ok(());
        }

        if event.owner == NONE {
            self.owned.remove(&event.selection);
            let _ = self.to_main.send(FromX::Cleared { target });
            return Ok(());
        }

        // An X client owns it now, so whatever we held for Wayland is gone.
        self.owned.remove(&event.selection);

        // Its own property: a target query and a paste can be in flight at once, and they would
        // otherwise overwrite each other's payload.
        self.conn
            .delete_property(self.window, self.atoms._SYNOIK_TARGETS)?;
        self.conn.convert_selection(
            self.window,
            event.selection,
            self.atoms.TARGETS,
            self.atoms._SYNOIK_TARGETS,
            event.timestamp,
        )?;
        Ok(())
    }

    fn on_selection_notify(&mut self, event: SelectionNotifyEvent) -> anyhow::Result<()> {
        let Some(target) = self.target_of(event.selection) else {
            return Ok(());
        };

        if event.target == self.atoms.TARGETS {
            if event.property == NONE {
                // The owner will not say what it has, so there is nothing to offer.
                let _ = self.to_main.send(FromX::Cleared { target });
                return Ok(());
            }
            let atoms = self.read_targets_property()?;
            let mime_types = self.mimes_from_targets(&atoms)?;
            if mime_types.is_empty() {
                let _ = self.to_main.send(FromX::Cleared { target });
            } else {
                let _ = self.to_main.send(FromX::Offer { target, mime_types });
            }
            return Ok(());
        }

        // Payload of a paste. Anything that is not the conversion we asked for is stale.
        if self.fetching.is_none() {
            return Ok(());
        }

        if event.property == NONE {
            // The owner refused; the pasting client gets an empty read rather than a hang.
            return self.finish_fetch();
        }

        let (ty, data) = self.read_transfer_property()?;
        if ty == self.atoms.INCR {
            // The owner will now write the payload in chunks, each announced by a PropertyNotify.
            if let Some(fetching) = self.fetching.as_mut() {
                fetching.incr = true;
                fetching.deadline = Instant::now() + TRANSFER_TIMEOUT;
            }
            return Ok(());
        }

        if let Some(fetching) = self.fetching.as_mut() {
            fetching.buf = data;
        }
        self.finish_fetch()
    }

    /// One chunk of an INCR transfer; a zero-length chunk ends it.
    fn on_property_notify(&mut self, event: PropertyNotifyEvent) -> anyhow::Result<()> {
        if event.window != self.window
            || event.atom != self.atoms._SYNOIK_SELECTION
            || event.state != Property::NEW_VALUE
        {
            return Ok(());
        }
        if !self.fetching.as_ref().is_some_and(|f| f.incr) {
            return Ok(());
        }

        let (_, data) = self.read_transfer_property()?;
        if data.is_empty() {
            return self.finish_fetch();
        }

        if let Some(fetching) = self.fetching.as_mut() {
            if fetching.buf.len() + data.len() > TRANSFER_LIMIT {
                warn!("X11 selection bridge: INCR transfer over {TRANSFER_LIMIT} bytes, dropping");
                fetching.buf.clear();
                return self.finish_fetch();
            }
            fetching.buf.extend_from_slice(&data);
        }
        Ok(())
    }

    fn mimes_from_targets(&mut self, atoms: &[Atom]) -> anyhow::Result<Vec<String>> {
        let mut mime_types: Vec<String> = Vec::new();
        for &atom in atoms {
            if atom == NONE {
                continue;
            }
            let name = match self.conn.get_atom_name(atom)?.reply() {
                Ok(reply) => String::from_utf8_lossy(&reply.name).into_owned(),
                // A target naming an atom that no longer exists: the owner's problem, not ours.
                Err(_) => continue,
            };
            for mime in mimes_for_atom(&self.atoms, atom, &name) {
                if !mime_types.contains(&mime) {
                    mime_types.push(mime);
                }
            }
        }
        Ok(mime_types)
    }

    fn on_selection_request(&mut self, event: SelectionRequestEvent) -> anyhow::Result<()> {
        let Some(target) = self.target_of(event.selection) else {
            return Ok(());
        };
        // A requestor from before ICCCM leaves the property unset, meaning "use the target".
        let property = if event.property == NONE {
            event.target
        } else {
            event.property
        };
        let serving = Serving {
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property,
            time: event.time,
        };

        let Some(owned) = self.owned.get(&event.selection) else {
            return self.refuse(&serving);
        };

        if event.target == self.atoms.TARGETS {
            let targets: Vec<u32> = owned.targets.clone();
            self.conn.change_property32(
                PropMode::REPLACE,
                serving.requestor,
                serving.property,
                AtomEnum::ATOM,
                &targets,
            )?;
            return self.notify(&serving, serving.property);
        }

        if event.target == self.atoms.TIMESTAMP {
            let since = [owned.since];
            self.conn.change_property32(
                PropMode::REPLACE,
                serving.requestor,
                serving.property,
                AtomEnum::INTEGER,
                &since,
            )?;
            return self.notify(&serving, serving.property);
        }

        // Which mime type does the requested target stand for? Exactly the inverse of what
        // `targets_for_mimes` advertised, or an X client is refused a target we offered it.
        let wanted = if event.target == self.atoms.TEXT || event.target == self.atoms.COMPOUND_TEXT
        {
            self.atoms.UTF8_STRING
        } else {
            event.target
        };
        let mime = owned
            .mime_types
            .iter()
            .find(|mime| {
                atom_for_mime(&self.atoms, mime) == Some(wanted)
                    || owned.mime_atoms.get(*mime) == Some(&wanted)
            })
            .cloned();

        let Some(mime_type) = mime else {
            return self.refuse(&serving);
        };

        let id = self.next_serving_id;
        self.next_serving_id += 1;
        self.serving.insert(id, serving);
        let _ = self.to_main.send(FromX::Request {
            target,
            id,
            mime_type,
        });
        Ok(())
    }

    fn on_selection_clear(&mut self, event: SelectionClearEvent) -> anyhow::Result<()> {
        // Somebody else took it; the XFixes notify that follows is what publishes the new owner.
        self.owned.remove(&event.selection);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// pipe helpers
// ---------------------------------------------------------------------------------------------

/// Read a peer's pipe to EOF, capped and deadlined.
///
/// The peer is another process, so neither the size nor the pace is ours to trust: a plain
/// blocking `read` on a client that opens the pipe and then stops never returns.
fn read_all(fd: OwnedFd) -> std::io::Result<Vec<u8>> {
    use smithay::reexports::rustix::event::{poll, PollFd, PollFlags, Timespec};

    fcntl_setfl(&fd, OFlags::NONBLOCK)?;

    let mut out = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let deadline = Instant::now() + TRANSFER_TIMEOUT;
    loop {
        match retry_on_intr(|| smithay::reexports::rustix::io::read(&fd, &mut chunk)) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                if out.len() + n > TRANSFER_LIMIT {
                    return Err(std::io::Error::other(
                        "selection data over the transfer limit",
                    ));
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(Errno::AGAIN) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(std::io::Error::other("selection transfer timed out"));
                }
                let timeout = Timespec {
                    tv_sec: left.as_secs() as _,
                    tv_nsec: left.subsec_nanos() as _,
                };
                let mut fds = [PollFd::from_borrowed_fd(fd.as_fd(), PollFlags::IN)];
                poll(&mut fds, Some(&timeout))?;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

fn write_all(fd: OwnedFd, data: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd);
    file.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atoms() -> Atoms {
        // The mapping is pure arithmetic on atom ids; any distinct set will do.
        Atoms {
            CLIPBOARD: 1,
            PRIMARY: 2,
            TARGETS: 3,
            TIMESTAMP: 4,
            MULTIPLE: 5,
            INCR: 6,
            UTF8_STRING: 7,
            STRING: 8,
            TEXT: 9,
            COMPOUND_TEXT: 10,
            _SYNOIK_SELECTION: 11,
            _SYNOIK_TARGETS: 12,
        }
    }

    #[test]
    fn text_mimes_map_to_the_x_text_targets() {
        let atoms = atoms();
        assert_eq!(
            atom_for_mime(&atoms, "text/plain;charset=utf-8"),
            Some(atoms.UTF8_STRING)
        );
        assert_eq!(atom_for_mime(&atoms, "text/plain"), Some(atoms.STRING));
        assert_eq!(atom_for_mime(&atoms, "image/png"), None);
    }

    #[test]
    fn x_isms_are_not_offered_as_mime_types() {
        let atoms = atoms();
        // A target with no slash in its name is an X-ism, not something a Wayland client can ask
        // for; offering it would put junk on the clipboard.
        assert!(mimes_for_atom(&atoms, 40, "SAVE_TARGETS").is_empty());
        assert!(mimes_for_atom(&atoms, atoms.TARGETS, "TARGETS").is_empty());
        assert_eq!(
            mimes_for_atom(&atoms, 41, "image/png"),
            vec!["image/png".to_owned()]
        );
    }

    #[test]
    fn the_locale_dependent_text_targets_are_offered_but_never_imported() {
        let atoms = atoms();
        // TEXT/COMPOUND_TEXT carry a locale-dependent encoding, so a Wayland client told
        // "text/plain" would get bytes it cannot interpret.
        assert!(mimes_for_atom(&atoms, atoms.TEXT, "TEXT").is_empty());
        assert!(mimes_for_atom(&atoms, atoms.COMPOUND_TEXT, "COMPOUND_TEXT").is_empty());

        // Offering them is fine: an X client asking for TEXT gets utf-8, which is what it means.
        let targets = targets_for_mimes(
            &atoms,
            &["text/plain;charset=utf-8".to_owned()],
            &HashMap::new(),
        );
        assert!(targets.contains(&atoms.TEXT));
        assert!(targets.contains(&atoms.COMPOUND_TEXT));
    }

    #[test]
    fn targets_always_lead_with_the_two_meta_targets() {
        let atoms = atoms();
        let targets = targets_for_mimes(&atoms, &[], &HashMap::new());
        assert_eq!(targets, vec![atoms.TARGETS, atoms.TIMESTAMP]);
    }

    #[test]
    fn a_non_text_mime_rides_its_interned_atom() {
        let atoms = atoms();
        let mut extra = HashMap::new();
        extra.insert("image/png".to_owned(), 99);
        let targets = targets_for_mimes(&atoms, &["image/png".to_owned()], &extra);
        assert!(targets.contains(&99));
        // No text offered, so no legacy text aliases.
        assert!(!targets.contains(&atoms.TEXT));
    }
}
