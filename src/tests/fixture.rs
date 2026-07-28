use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use niri_config::Config;
use smithay::backend::input::{ButtonState, InputEvent, KeyState, Keycode};
use smithay::output::Output;
use smithay::utils::{Logical, Rectangle};

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

/// Pin the collation locale for the whole test process.
///
/// `LC_COLLATE` is a process global that the test binary — which never runs `main`, and so
/// never calls [`crate::gnome::init_collation`] — would otherwise leave at C, where sorting
/// is codepoint order and "Utilities" comes before "archive". Every test that asserts on
/// the app grid's order depends on which it is, so it is pinned rather than inherited:
/// `en_US.UTF-8` if the machine has it, else whatever the environment says.
fn pin_collation() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: inside a `Once`, before the fixture hands out any state.
        unsafe {
            for locale in [
                c"en_US.UTF-8".as_ptr(),
                c"en_US.utf8".as_ptr(),
                c"".as_ptr(),
            ] {
                if !libc::setlocale(libc::LC_COLLATE, locale).is_null() {
                    return;
                }
            }
        }
    });
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_config(Config::default())
    }

    pub fn with_config(config: Config) -> Self {
        pin_collation();
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

    /// Pump the compositor's own event loop until `pred` holds, giving up after
    /// `timeout`. This spends **real** wall-clock time: it is for the handful of
    /// behaviours driven by a calloop timer rather than by the animation clock (drag
    /// countdowns), which no amount of `niri_complete_animations` will fire. Returns
    /// whether the predicate came true.
    pub fn dispatch_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&mut crate::niri::State) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.state.server.dispatch();
            if pred(&mut self.state.server.state) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
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

    /// Drive animations to completion by pinning the (lazy) clock forward past any
    /// running animation, then advancing. Unlike [`niri_complete_animations`], this
    /// actually moves the clock, so `is_done`/progress read as finished at render
    /// time too — the correct way to settle a timed overlay (see the
    /// headless-animation-clock trap). Call it immediately before asserting, after
    /// the last input roundtrip (`refresh` clears the lazy clock).
    pub fn settle_animations(&mut self) {
        let niri = self.niri();
        let now = niri.clock.now_unadjusted();
        niri.clock.set_unadjusted(now + Duration::from_millis(1000));
        niri.advance_animations();
    }

    /// Sample `f` at `n + 1` evenly spaced instants across the next `duration`,
    /// advancing animations at each pinned instant — the animated analogue of
    /// [`settle_animations`](Self::settle_animations), for asserting what a UI
    /// looks like *during* a transition rather than at its ends.
    ///
    /// Trigger the transition **before** calling, and do not dispatch or round-trip
    /// clients inside `f`: a roundtrip clears the lazy clock and re-times every
    /// running animation (the headless-animation-clock trap). The clock only ever
    /// moves here, by exact fractions of `duration`, so a sample is a pure function
    /// of pinned time and the series is reproducible.
    ///
    /// For a spring (whose duration isn't a constant), pass a generous span and let
    /// the tail samples be settled — every invariant worth asserting holds trivially
    /// over a settled tail.
    pub fn sample_animation<T>(
        &mut self,
        duration: Duration,
        n: u32,
        mut f: impl FnMut(&mut Self) -> T,
    ) -> Vec<T> {
        let start = self.niri().clock.now_unadjusted();
        (0..=n)
            .map(|i| {
                let at = start + duration.mul_f64(f64::from(i) / f64::from(n));
                let niri = self.niri();
                niri.clock.set_unadjusted(at);
                niri.advance_animations();
                f(self)
            })
            .collect()
    }

    /// [`sample_animation`](Self::sample_animation) of the one geometry the
    /// overview's workspace row is: every workspace's render rect on output `n`,
    /// which is what rendering, hit-testing and drop targets all consume.
    pub fn sample_workspace_geo(
        &mut self,
        output_n: u8,
        duration: Duration,
        n: u32,
    ) -> Vec<Vec<Rectangle<f64, Logical>>> {
        let output = self.niri_output(output_n);
        self.sample_animation(duration, n, |f| {
            f.niri()
                .layout
                .monitor_for_output(&output)
                .unwrap()
                .workspaces_render_geo()
                .collect()
        })
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
                finger: None,
            },
        };
        self.niri_state().process_input_event(event);
    }

    /// Inject a continuous (touchpad) scroll of `(dx, dy)` through the real input
    /// pipeline. A `(0., 0.)` event is the gesture end libinput sends when the fingers
    /// lift — which is how [`crate::input::scroll_swipe_gesture`] knows a swipe is over.
    pub fn scroll_finger(&mut self, dx: f64, dy: f64) {
        let event = InputEvent::<SyntheticInputBackend>::PointerAxis {
            event: SyntheticPointerAxisEvent {
                time: self.next_input_micros(),
                v120: 0.,
                finger: Some((dx, dy)),
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

    /// Skip the synthetic input clock forward, so the next injected event carries
    /// a timestamp `ms` milliseconds later than it otherwise would. Input events
    /// are normally stamped 1 ms apart; this is how a test drives a behavior that
    /// keys off the *gap* between two events (e.g. a double-tap window).
    pub fn advance_input_time(&mut self, ms: u32) {
        self.next_input_time += ms;
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
