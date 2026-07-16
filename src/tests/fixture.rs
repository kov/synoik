use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use niri_config::Config;
use smithay::backend::input::{ButtonState, InputEvent, KeyState, Keycode};
use smithay::output::Output;

use super::client::{Client, ClientId};
use super::server::Server;
use crate::input::synthetic::{
    SyntheticInputBackend, SyntheticKeyboardKeyEvent, SyntheticPointerAxisEvent,
    SyntheticPointerButtonEvent, SyntheticPointerMotionEvent, SyntheticTouchDownEvent,
    SyntheticTouchUpEvent,
};
use crate::niri::{NewClient, Niri};

pub struct Fixture {
    pub event_loop: EventLoop<'static, State>,
    pub handle: LoopHandle<'static, State>,
    pub state: State,
    /// Monotonic timestamp (ms) handed to each synthesized input event.
    next_input_time: u32,
}

pub struct State {
    pub server: Server,
    pub clients: Vec<Client>,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();

        let server = Server::new(config);
        let fd = server.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        handle
            .insert_source(source, |_, _, state: &mut State| {
                state.server.dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();

        let state = State {
            server,
            clients: Vec::new(),
        };

        Self {
            event_loop,
            handle,
            state,
            next_input_time: 0,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();
    }

    pub fn niri_state(&mut self) -> &mut crate::niri::State {
        &mut self.state.server.state
    }

    pub fn niri(&mut self) -> &mut Niri {
        &mut self.niri_state().niri
    }

    pub fn niri_output(&self, n: u8) -> Output {
        let niri = &self.state.server.state.niri;
        let idx = usize::from(n - 1);
        let output = niri.global_space.outputs().nth(idx).unwrap();
        output.clone()
    }

    pub fn niri_focus_output(&mut self, n: u8) {
        let niri = &mut self.state.server.state.niri;
        let idx = usize::from(n - 1);
        let output = niri.global_space.outputs().nth(idx).unwrap();
        niri.layout.focus_output(output);
    }

    pub fn niri_complete_animations(&mut self) {
        let niri = self.niri();
        niri.clock.set_complete_instantly(true);
        niri.advance_animations();
        niri.clock.set_complete_instantly(false);
    }

    /// Inject a key press through the real input pipeline (`process_input_event`).
    ///
    /// `evdev_code` is a Linux `KEY_*` evdev keycode (e.g. `KEY_LEFTMETA`); it is
    /// translated to the X11 keycode space (evdev + 8) that the keyboard expects,
    /// then mapped to a keysym by the seat's xkb keymap, exactly as a libinput
    /// event would be.
    pub fn key_press(&mut self, evdev_code: u32) {
        self.key_event(evdev_code, KeyState::Pressed);
    }

    /// Inject a key release through the real input pipeline. See [`key_press`].
    ///
    /// [`key_press`]: Self::key_press
    pub fn key_release(&mut self, evdev_code: u32) {
        self.key_event(evdev_code, KeyState::Released);
    }

    fn key_event(&mut self, evdev_code: u32, state: KeyState) {
        let event = InputEvent::<SyntheticInputBackend>::Keyboard {
            event: SyntheticKeyboardKeyEvent {
                time: self.next_input_micros(),
                key_code: Keycode::new(evdev_code + 8),
                state,
            },
        };
        self.niri_state().process_input_event(event);
    }

    /// Inject a pointer button event through the real input pipeline.
    ///
    /// `button_code` is a Linux `BTN_*` evdev code (e.g. `BTN_LEFT`).
    pub fn pointer_button(&mut self, button_code: u32, state: ButtonState) {
        let event = InputEvent::<SyntheticInputBackend>::PointerButton {
            event: SyntheticPointerButtonEvent {
                time: self.next_input_micros(),
                button_code,
                state,
            },
        };
        self.niri_state().process_input_event(event);
    }

    /// Inject relative pointer motion through the real input pipeline.
    pub fn pointer_motion(&mut self, dx: f64, dy: f64) {
        let event = InputEvent::<SyntheticInputBackend>::PointerMotion {
            event: SyntheticPointerMotionEvent {
                time: self.next_input_micros(),
                dx,
                dy,
            },
        };
        self.niri_state().process_input_event(event);
    }

    /// Inject one vertical wheel notch (a discrete scroll) through the real
    /// input pipeline.
    pub fn scroll_wheel(&mut self) {
        let event = InputEvent::<SyntheticInputBackend>::PointerAxis {
            event: SyntheticPointerAxisEvent {
                time: self.next_input_micros(),
                v120: 120.0,
            },
        };
        self.niri_state().process_input_event(event);
    }

    /// Inject a touch-down at `(x, y)` through the real input pipeline.
    pub fn touch_down(&mut self, x: f64, y: f64) {
        let event = InputEvent::<SyntheticInputBackend>::TouchDown {
            event: SyntheticTouchDownEvent {
                time: self.next_input_micros(),
                x,
                y,
            },
        };
        self.niri_state().process_input_event(event);
    }

    /// Inject a touch-up (for the single test slot) through the real input
    /// pipeline.
    pub fn touch_up(&mut self) {
        let event = InputEvent::<SyntheticInputBackend>::TouchUp {
            event: SyntheticTouchUpEvent {
                time: self.next_input_micros(),
            },
        };
        self.niri_state().process_input_event(event);
    }

    fn next_input_micros(&mut self) -> u64 {
        let time = self.next_input_time;
        self.next_input_time += 1;
        u64::from(time) * 1000 // micros, as libinput reports
    }

    pub fn add_output(&mut self, n: u8, size: (u16, u16)) {
        let state = self.niri_state();
        let niri = &mut state.niri;
        state.backend.headless().add_output(niri, n, size);
    }

    pub fn add_client(&mut self) -> ClientId {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        self.niri().insert_client(NewClient {
            client: sock1,
            restricted: false,
            credentials_unknown: false,
        });

        let client = Client::new(sock2);
        let id = client.id;

        let fd = client.event_loop.as_fd().try_clone_to_owned().unwrap();
        let source = Generic::new(fd, Interest::READ, Mode::Level);
        self.handle
            .insert_source(source, move |_, _, state: &mut State| {
                state.client(id).dispatch();
                Ok(PostAction::Continue)
            })
            .unwrap();

        self.state.clients.push(client);
        self.roundtrip(id);
        id
    }

    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.state.client(id)
    }

    pub fn roundtrip(&mut self, id: ClientId) {
        let client = self.state.client(id);
        let data = client.send_sync();
        while !data.done.load(Ordering::Relaxed) {
            self.dispatch();
        }
    }

    /// Roundtrip twice in a row.
    ///
    /// For some reason, when running tests on many threads at once, a single roundtrip is
    /// sometimes not sufficient to get the configure events to the client.
    ///
    /// I suspect that this is because these configure events are sent from the niri loop callback,
    /// so they arrive after the sync done event and don't get processed in that client dispatch
    /// cycle. I'm not sure why this would be dependent on multithreading. But if this is indeed
    /// the issue, then a double roundtrip fixes it.
    pub fn double_roundtrip(&mut self, id: ClientId) {
        self.roundtrip(id);
        self.roundtrip(id);
    }
}

impl State {
    pub fn client(&mut self, id: ClientId) -> &mut Client {
        self.clients.iter_mut().find(|c| c.id == id).unwrap()
    }
}
