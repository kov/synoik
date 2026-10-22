// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Where keyboard focus and the text caret are, from the accessibility bus — gnome-shell's
//! `FocusCaretTracker` (`js/ui/focusCaretTracker.js`) and the half of `ZoomRegion` that reads
//! what it reports (`_updateFocus`, `_updateCaret`, `_convertExtentsToScreenSpace`,
//! `magnifier.js:861-972`). The magnifier follows them.
//!
//! An AT-SPI client over plain zbus: apps only emit the events someone registered for, so the
//! tracker registers `object:state-changed:focused` / `:selected` and `object:text-caret-moved`
//! with the registry while the magnifier wants them, listens for them, and asks the emitting
//! object where it is — in window coordinates, which the compositor turns into the screen's
//! with the focused window's rect. Every query goes to an app's process, so it all happens on
//! its own threads, with `Atspi.set_timeout(250, 250)`'s patience.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use calloop::{LoopHandle, RegistrationToken};
use smithay::utils::{Logical, Rectangle};
use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure};
use zbus::MatchRule;

use crate::synoik::State;

const FOCUS_EVENTS: [&str; 2] = [
    "object:state-changed:focused",
    "object:state-changed:selected",
];
const CARET_EVENT: &str = "object:text-caret-moved";

/// `Atspi.set_timeout(250, 250)`.
const TIMEOUT: Duration = Duration::from_millis(250);

// `atspi-constants.h`.
const ROLE_DIALOG: u32 = 16;
const ROLE_FRAME: u32 = 23;
const ROLE_WINDOW: u32 = 69;
const ROLE_APPLICATION: u32 = 75;
const STATE_ACTIVE: u32 = 1;
const STATE_FOCUSED: u32 = 12;
const COORD_TYPE_WINDOW: u32 = 1;

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";

/// What moved, in the focused window's coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tracked {
    /// The focused widget's extents.
    Focus(Rectangle<i32, Logical>),
    /// The caret's character extents.
    Caret(Rectangle<i32, Logical>),
}

enum Command {
    Register(&'static str),
    Deregister(&'static str),
    /// Nothing: whether it can be sent says whether the thread is alive.
    Ping,
}

/// The tracker, started on first use and kept for the session.
#[derive(Default)]
pub struct FocusCaretTracker {
    commands: Option<mpsc::Sender<Command>>,
    /// The loop's end of the events channel, removed when the tracker restarts.
    source: Option<RegistrationToken>,
    focus: bool,
    caret: bool,
}

impl std::fmt::Debug for FocusCaretTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FocusCaretTracker")
            .field("started", &self.commands.is_some())
            .field("focus", &self.focus)
            .field("caret", &self.caret)
            .finish()
    }
}

impl FocusCaretTracker {
    /// Listen for focus and caret moves as asked (`register*Listener` /
    /// `deregister*Listener`).
    pub fn set_listening(
        &mut self,
        focus: bool,
        caret: bool,
        event_loop: &LoopHandle<'static, State>,
    ) {
        if focus == self.focus && caret == self.caret {
            return;
        }
        // A test must never register events with whatever accessibility bus the machine has.
        if cfg!(test) {
            (self.focus, self.caret) = (focus, caret);
            return;
        }
        // A thread that could not reach the bus has gone, and the next request starts over.
        if self
            .commands
            .as_ref()
            .is_some_and(|c| c.send(Command::Ping).is_err())
        {
            self.commands = None;
            (self.focus, self.caret) = (false, false);
        }
        if self.commands.is_none() {
            if !focus && !caret {
                return;
            }
            if let Some(token) = self.source.take() {
                event_loop.remove(token);
            }
            let Some((commands, source)) = start(event_loop) else {
                return;
            };
            self.commands = Some(commands);
            self.source = Some(source);
        }
        let Some(commands) = &self.commands else {
            return;
        };
        let send = |command| {
            let _ = commands.send(command);
        };
        if focus != self.focus {
            for event in FOCUS_EVENTS {
                send(if focus {
                    Command::Register(event)
                } else {
                    Command::Deregister(event)
                });
            }
        }
        if caret != self.caret {
            send(if caret {
                Command::Register(CARET_EVENT)
            } else {
                Command::Deregister(CARET_EVENT)
            });
        }
        self.focus = focus;
        self.caret = caret;
    }
}

impl FocusCaretTracker {
    /// Whether focus and caret moves are listened for.
    pub fn listening(&self) -> (bool, bool) {
        (self.focus, self.caret)
    }
}

/// Connect to the accessibility bus on a thread of its own, which then (de)registers events as
/// told; a second thread reads the events and sends what moved to the loop.
fn start(
    event_loop: &LoopHandle<'static, State>,
) -> Option<(mpsc::Sender<Command>, RegistrationToken)> {
    let (to_loop, from_tracker) = calloop::channel::channel::<Tracked>();
    let (commands, from_main) = mpsc::channel::<Command>();
    let res = thread::Builder::new()
        .name("FocusCaretTracker".to_owned())
        .spawn(move || {
            let conn = match connect() {
                Ok(conn) => conn,
                Err(err) => {
                    warn!("focus and caret tracking: no accessibility bus: {err:?}");
                    return;
                }
            };
            let reader = conn.clone();
            let spawned = thread::Builder::new()
                .name("FocusCaretTracker events".to_owned())
                .spawn(move || read_events(&reader, &to_loop));
            if let Err(err) = spawned {
                warn!("focus and caret tracking: no event thread: {err:?}");
                return;
            }
            while let Ok(command) = from_main.recv() {
                let res = match command {
                    Command::Register(event) => register(&conn, event),
                    // `(s event, s app_bus_name)`; libatspi passes an empty name.
                    Command::Deregister(event) => conn
                        .call_method(
                            Some("org.a11y.atspi.Registry"),
                            "/org/a11y/atspi/registry",
                            Some("org.a11y.atspi.Registry"),
                            "DeregisterEvent",
                            &(event, ""),
                        )
                        .map(drop),
                    Command::Ping => Ok(()),
                };
                if let Err(err) = res {
                    warn!("focus and caret tracking: (de)registering an event: {err:?}");
                }
            }
        });
    if let Err(err) = res {
        warn!("focus and caret tracking: no thread: {err:?}");
        return None;
    }
    // Only now: a source inserted for a thread that never ran would sit in the loop for good.
    // Should this fail, dropping `commands` ends the thread.
    match event_loop.insert_source(from_tracker, |event, _, state| {
        if let calloop::channel::Event::Msg(tracked) = event {
            state.magnifier_tracked(tracked);
        }
    }) {
        Ok(token) => Some((commands, token)),
        Err(err) => {
            warn!("focus and caret tracking: no event source: {err:?}");
            None
        }
    }
}

fn connect() -> zbus::Result<Connection> {
    let session = Connection::session()?;
    let address: String = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )?
        .body()
        .deserialize()?;
    zbus::blocking::connection::Builder::address(address.as_str())?
        .method_timeout(TIMEOUT)
        .build()
}

/// `RegisterEvent`, with the properties and app arguments the registry has taken since 2.46,
/// or the bare event an older one takes.
fn register(conn: &Connection, event: &str) -> zbus::Result<()> {
    let call = |body: &dyn Fn() -> zbus::Result<zbus::Message>| body().map(drop);
    call(&|| {
        conn.call_method(
            Some("org.a11y.atspi.Registry"),
            "/org/a11y/atspi/registry",
            Some("org.a11y.atspi.Registry"),
            "RegisterEvent",
            &(event, Vec::<String>::new(), ""),
        )
    })
    .or_else(|_| {
        call(&|| {
            conn.call_method(
                Some("org.a11y.atspi.Registry"),
                "/org/a11y/atspi/registry",
                Some("org.a11y.atspi.Registry"),
                "RegisterEvent",
                &(event,),
            )
        })
    })
}

fn read_events(conn: &Connection, to_loop: &calloop::channel::Sender<Tracked>) {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.a11y.atspi.Event.Object")
        .map(|rule| rule.build());
    let iter = match rule.and_then(|rule| MessageIterator::for_match_rule(rule, conn, Some(64))) {
        Ok(iter) => iter,
        Err(err) => {
            warn!("focus and caret tracking: cannot listen: {err:?}");
            return;
        }
    };
    for msg in iter {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        let (Some(sender), Some(path), Some(member)) =
            (header.sender(), header.path(), header.member())
        else {
            continue;
        };
        let object = Object {
            conn,
            bus: sender.to_string(),
            path: path.to_string(),
        };
        let body = msg.body();
        let Ok(body) = body.deserialize::<Structure>() else {
            continue;
        };
        let fields = body.fields();
        let kind = fields
            .first()
            .and_then(|f| String::try_from(f.try_clone().ok()?).ok())
            .unwrap_or_default();
        let detail1 = fields
            .get(1)
            .and_then(|f| i32::try_from(f.try_clone().ok()?).ok())
            .unwrap_or_default();
        let tracked = match member.as_str() {
            // `_onChanged` takes `object:state-changed:*`; `_updateFocus` wants a gain.
            "StateChanged" if (kind == "focused" || kind == "selected") && detail1 == 1 => {
                object.focus_extents().map(Tracked::Focus)
            }
            "TextCaretMoved" => object.caret_extents().map(Tracked::Caret),
            _ => None,
        };
        if let Some(tracked) = tracked {
            if to_loop.send(tracked).is_err() {
                return;
            }
        }
    }
}

/// One accessible object, addressed by its app's bus name and its path.
struct Object<'a> {
    conn: &'a Connection,
    bus: String,
    path: String,
}

impl<'a> Object<'a> {
    fn call<B, R>(&self, iface: &str, method: &str, body: &B) -> Option<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        self.conn
            .call_method(
                Some(self.bus.as_str()),
                self.path.as_str(),
                Some(iface),
                method,
                body,
            )
            .ok()?
            .body()
            .deserialize()
            .ok()
    }

    fn property(&self, iface: &str, name: &str) -> Option<OwnedValue> {
        self.call("org.freedesktop.DBus.Properties", "Get", &(iface, name))
    }

    fn role(&self) -> Option<u32> {
        self.call(ACCESSIBLE, "GetRole", &())
    }

    fn has_state(&self, state: u32) -> bool {
        let words: Option<Vec<u32>> = self.call(ACCESSIBLE, "GetState", &());
        words
            .and_then(|w| w.get((state / 32) as usize).copied())
            .is_some_and(|word| word & (1 << (state % 32)) != 0)
    }

    fn parent(&self) -> Option<Object<'a>> {
        let value = self.property(ACCESSIBLE, "Parent")?;
        let (bus, path): (String, OwnedObjectPath) = value.try_into().ok()?;
        (path.as_str() != "/org/a11y/atspi/null").then(|| Object {
            conn: self.conn,
            bus,
            path: path.to_string(),
        })
    }

    /// `_convertExtentsToScreenSpace`'s checks: only the focused widget of the active window
    /// counts, and never the shell's own.
    fn is_focused_in_active_window(&self) -> bool {
        let mut window: Option<Object<'a>> = None;
        let mut app_name = None;
        // From the accessible itself: a window that reports the focus is its own window.
        let mut iter = Some(Object {
            conn: self.conn,
            bus: self.bus.clone(),
            path: self.path.clone(),
        });
        let mut depth = 0;
        while let Some(object) = iter {
            depth += 1;
            if depth > 64 {
                return false;
            }
            match object.role() {
                Some(ROLE_APPLICATION) => {
                    app_name = object
                        .property(ACCESSIBLE, "Name")
                        .and_then(|v| String::try_from(v).ok());
                    break;
                }
                Some(ROLE_FRAME | ROLE_DIALOG | ROLE_WINDOW) => {
                    iter = object.parent();
                    window = Some(object);
                }
                _ => iter = object.parent(),
            }
        }
        if app_name.as_deref() == Some("synoik") {
            return false;
        }
        window.is_some_and(|w| w.has_state(STATE_ACTIVE)) && self.has_state(STATE_FOCUSED)
    }

    fn focus_extents(&self) -> Option<Rectangle<i32, Logical>> {
        if !self.is_focused_in_active_window() {
            return None;
        }
        let (x, y, w, h): (i32, i32, i32, i32) = self.call(
            "org.a11y.atspi.Component",
            "GetExtents",
            &(COORD_TYPE_WINDOW,),
        )?;
        Some(Rectangle::new((x, y).into(), (w, h).into()))
    }

    fn caret_extents(&self) -> Option<Rectangle<i32, Logical>> {
        let offset = i32::try_from(self.property("org.a11y.atspi.Text", "CaretOffset")?).ok()?;
        let (x, y, w, h): (i32, i32, i32, i32) = self.call(
            "org.a11y.atspi.Text",
            "GetCharacterExtents",
            &(offset, COORD_TYPE_WINDOW),
        )?;
        if !self.is_focused_in_active_window() {
            return None;
        }
        Some(Rectangle::new((x, y).into(), (w, h).into()))
    }
}
