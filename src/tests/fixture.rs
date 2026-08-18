// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

use calloop::generic::Generic;
use calloop::{EventLoop, Interest, LoopHandle, Mode, PostAction};
use smithay::backend::input::{ButtonState, InputEvent, KeyState, Keycode};
use smithay::output::Output;
use smithay::utils::{Logical, Rectangle};
use synoik_config::Config;

use super::client::{Client, ClientId};
use super::server::Server;
use crate::input::synthetic::{
    SyntheticInputBackend, SyntheticKeyboardKeyEvent, SyntheticPointerAxisEvent,
    SyntheticPointerButtonEvent, SyntheticPointerMotionEvent, SyntheticTouchDownEvent,
    SyntheticTouchUpEvent,
};
use crate::synoik::{NewClient, Synoik};

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
///
/// `setlocale` succeeding is not enough — a machine without the `en_US` locale data falls
/// through to the environment, and a bare container's environment *is* C. That used to
/// leave the app-grid order tests silently asserting codepoint order, which is how eleven
/// folder tests failed on the fedora job alone. So the *property* is checked, not the
/// locale's name: panic here, where the message names the missing dependency, rather than
/// eleven assertions away where it looks like a folder bug.
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
                if libc::setlocale(libc::LC_COLLATE, locale).is_null() {
                    continue;
                }
                // Dictionary order puts "a" before "Utilities"; codepoint order does not
                // ('U' is 0x55, 'a' is 0x61). This is exactly the comparison the app grid
                // makes, so it is the one worth proving.
                if libc::strcoll(c"a".as_ptr(), c"Utilities".as_ptr()) < 0 {
                    return;
                }
            }
        }

        panic!(
            "no locale on this machine collates \"a\" before \"Utilities\" — the app grid's \
             name order would be codepoint order, and every test that asserts on it would \
             assert the wrong thing. Install the en_US locale data (fedora: glibc-langpack-en, \
             debian/ubuntu: locales + `locale-gen en_US.UTF-8`)."
        );
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
    /// countdowns), which no amount of `synoik_complete_animations` will fire. Returns
    /// whether the predicate came true.
    pub fn dispatch_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&mut crate::synoik::State) -> bool,
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

    pub fn synoik_state(&mut self) -> &mut crate::synoik::State {
        &mut self.state.server.state
    }

    /// Run the compositor's reconcile phase (`State::refresh`) — and **nothing else**: no
    /// animation advance, no redraw.
    ///
    /// This, not a render, is where UI state that *tracks* the layout is brought in line (the
    /// panel's Activities highlight, a panel menu the overview opens over). Reach for it after
    /// driving an action directly with `do_action`, which does not pump the loop.
    ///
    /// Deliberately not `refresh_and_flush_clients`: that redraws too, so a test using it would
    /// pass whether the state were synced here or at render time — which is exactly the confusion
    /// that let the Activities highlight sit one cycle late.
    pub fn refresh(&mut self) {
        self.synoik_state().refresh();
    }

    pub fn synoik(&mut self) -> &mut Synoik {
        &mut self.synoik_state().synoik
    }

    /// Give the compositor a [`StubAudio`] backend bound at `volume` (unmuted) and return a handle
    /// to it. Without this, `audio_backend` is `None` — the honest headless default, since there is
    /// no PipeWire — and every audio path silently no-ops, which is exactly how the panel-scroll
    /// OSD went unpinned for so long.
    ///
    /// Also seeds `synoik.audio`, the model the panel icon and the OSD read: the live backend
    /// publishes it over its calloop channel, and nothing drives that channel here.
    pub fn install_stub_audio(&mut self, volume: f64) -> crate::audio::StubAudio {
        let status = crate::audio::AudioStatus {
            volume,
            muted: false,
        };
        let stub = crate::audio::StubAudio::with_status(status);
        self.synoik().audio_backend = Some(Box::new(stub.clone()));
        // Through the real entry point, not by assigning the field: `on_audio_status` is what also
        // puts the volume icon in the panel's status cluster, and without it there is nothing to
        // aim a scroll at.
        self.synoik_state().on_audio_status(Some(status));
        stub
    }

    /// Everything the shield has asked gdm for since the last call, in order.
    ///
    /// The verifier task is a real channel in the fixture (see `Server::new`), so this is what a
    /// live gdm client would have received — the only way to see requests like `StartFingerprint`,
    /// which change nothing observable inside the compositor.
    pub fn gdm_requests(&mut self) -> Vec<crate::dbus::gdm::VerifierRequest> {
        let mut out = Vec::new();
        while let Ok(request) = self.state.server.gdm_requests.try_recv() {
            out.push(request);
        }
        out
    }

    pub fn synoik_output(&self, n: u8) -> Output {
        let synoik = &self.state.server.state.synoik;
        let idx = usize::from(n - 1);
        let output = synoik.global_space.outputs().nth(idx).unwrap();
        output.clone()
    }

    pub fn synoik_focus_output(&mut self, n: u8) {
        let synoik = &mut self.state.server.state.synoik;
        let idx = usize::from(n - 1);
        let output = synoik.global_space.outputs().nth(idx).unwrap();
        synoik.layout.focus_output(output);
    }

    pub fn synoik_complete_animations(&mut self) {
        let synoik = self.synoik();
        synoik.clock.set_complete_instantly(true);
        synoik.advance_animations();
        synoik.clock.set_complete_instantly(false);
    }

    /// Drive animations to completion by pinning the (lazy) clock forward past any
    /// running animation, then advancing. Unlike [`synoik_complete_animations`], this
    /// actually moves the clock, so `is_done`/progress read as finished at render
    /// time too — the correct way to settle a timed overlay (see the
    /// headless-animation-clock trap). Call it immediately before asserting, after
    /// the last input roundtrip (`refresh` clears the lazy clock).
    pub fn settle_animations(&mut self) {
        let synoik = self.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik
            .clock
            .set_unadjusted(now + Duration::from_millis(1000));
        synoik.advance_animations();
    }

    /// Run the compositor the way the live session runs it — one frame at a time — until nothing
    /// is animating.
    ///
    /// The primitives around this one each do a *part* of a frame:
    /// [`synoik_complete_animations`](Self::synoik_complete_animations) teleports the animation
    /// clock to the end without a reconcile, [`refresh`](Self::refresh) reconciles without
    /// advancing, [`dispatch`](Self::dispatch) pumps the loop without either. A test that needs a
    /// transition to *finish the way it finishes in the session* had to hand-assemble the
    /// sequence, and every test assembled it slightly differently — which is how a behaviour that
    /// lands on the frame after an animation ends can be invisible to the corpus.
    ///
    /// This is that sequence, in the order the real loop runs it: advance the clock by one refresh
    /// interval, pump the event loop, reconcile. It gives up after `max_frames` so a transition
    /// that never settles fails the test instead of hanging it.
    pub fn run_until_settled(&mut self, max_frames: usize) -> bool {
        const FRAME: Duration = Duration::from_micros(16_667);

        // Freezing is how the frames get a size that is a property of the test rather than of the
        // machine, but it must not outlive the call: a caller that never froze goes on to expect a
        // clock that follows real time, and calloop-timer behaviour would silently never fire.
        let was_frozen = self.synoik().clock.is_frozen();
        self.freeze_clock();
        let mut settled = false;
        for _ in 0..max_frames {
            self.advance_clock(FRAME);
            self.dispatch();
            self.refresh();
            if !self.transitions_ongoing() {
                // One more full frame: the live loop reconciles *after* the last animated frame,
                // and that trailing pass is where anything deferred to the end of a transition
                // actually lands.
                self.advance_clock(FRAME);
                self.dispatch();
                self.refresh();
                settled = true;
                break;
            }
        }
        if !was_frozen {
            self.synoik().clock.unfreeze();
        }
        settled
    }

    /// [`run_until_settled`](Self::run_until_settled) with a generous cap, failing the test if the
    /// transition never ends. This is the one to reach for: a test almost never wants to *tolerate*
    /// a transition that will not finish.
    pub fn settle(&mut self) {
        assert!(
            self.run_until_settled(600),
            "transitions still running after 10s of frames",
        );
    }

    /// Whether any monitor still has a transition running.
    pub fn transitions_ongoing(&mut self) -> bool {
        self.synoik()
            .layout
            .monitors()
            .any(|monitor| monitor.are_transitions_ongoing())
    }

    /// Hold the animation clock still, so that only [`advance_clock`](Self::advance_clock) moves
    /// it — the way to sample a mid-animation state across round trips.
    ///
    /// [`sample_animation`](Self::sample_animation) pins the clock per sample, which is enough when
    /// nothing dispatches in between; a round trip runs the event loop, whose last act is to clear
    /// the lazy clock, so the next read comes from the monotonic clock and the animation jumps
    /// ahead by however long the round trip took in real time. That made every "assert something
    /// mid-resize" test a race against the machine: under load
    /// (two concurrent suites) the animation settled early and the precondition tripped.
    pub fn freeze_clock(&mut self) {
        self.synoik().clock.freeze();
    }

    /// Move the frozen clock forward by `by` and re-time every running animation.
    ///
    /// Only meaningful after [`freeze_clock`](Self::freeze_clock) — without it the next event-loop
    /// iteration discards the pinned time.
    pub fn advance_clock(&mut self, by: Duration) {
        let synoik = self.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik.clock.set_unadjusted(now + by);
        synoik.advance_animations();
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
        let start = self.synoik().clock.now_unadjusted();
        (0..=n)
            .map(|i| {
                let at = start + duration.mul_f64(f64::from(i) / f64::from(n));
                let synoik = self.synoik();
                synoik.clock.set_unadjusted(at);
                synoik.advance_animations();
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
        let output = self.synoik_output(output_n);
        self.sample_animation(duration, n, |f| {
            f.synoik()
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
        self.synoik_state().process_input_event(event);
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
        self.synoik_state().process_input_event(event);
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
        self.synoik_state().process_input_event(event);
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
        self.synoik_state().process_input_event(event);
    }

    /// Inject one vertical wheel notch in the *other* direction (scroll up).
    pub fn scroll_wheel_up(&mut self) {
        let event = InputEvent::<SyntheticInputBackend>::PointerAxis {
            event: SyntheticPointerAxisEvent {
                time: self.next_input_micros(),
                v120: -120.0,
                finger: None,
            },
        };
        self.synoik_state().process_input_event(event);
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
        self.synoik_state().process_input_event(event);
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
        self.synoik_state().process_input_event(event);
    }

    /// Inject a touch-up (for the single test slot) through the real input
    /// pipeline.
    pub fn touch_up(&mut self) {
        let event = InputEvent::<SyntheticInputBackend>::TouchUp {
            event: SyntheticTouchUpEvent {
                time: self.next_input_micros(),
            },
        };
        self.synoik_state().process_input_event(event);
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
        let state = self.synoik_state();
        let synoik = &mut state.synoik;
        state.backend.headless().add_output(synoik, n, size);
    }

    /// Change an existing output's mode and/or fractional scale, the way an EDID/mode change or a
    /// `ApplyMonitorsConfig` does, and run the resize through `Synoik::output_resized`.
    ///
    /// That last step is the point: it is what recomputes the working area and re-lays-out the
    /// windows, so a test that only calls `change_current_state` measures nothing.
    pub fn resize_output(&mut self, n: u8, size: Option<(u16, u16)>, scale: Option<f64>) {
        let output = self.synoik_output(n);
        let mode = size.map(|(w, h)| smithay::output::Mode {
            size: smithay::utils::Size::from((i32::from(w), i32::from(h))),
            refresh: 60_000,
        });
        output.change_current_state(
            mode,
            None,
            scale.map(smithay::output::Scale::Fractional),
            None,
        );
        if let Some(mode) = mode {
            output.set_preferred(mode);
        }
        self.synoik().output_resized(&output);
    }

    /// Unplug a headless output added by [`add_output`](Self::add_output), so a test
    /// can exercise the per-output teardown paths (OSD windows, banner retargeting).
    pub fn remove_output(&mut self, n: u8) {
        let output = self.synoik_output(n);
        self.synoik().remove_output(&output);
    }

    pub fn add_client(&mut self) -> ClientId {
        let (sock1, sock2) = UnixStream::pair().unwrap();
        self.synoik().insert_client(NewClient {
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
    /// I suspect that this is because these configure events are sent from the synoik loop
    /// callback, so they arrive after the sync done event and don't get processed in that
    /// client dispatch cycle. I'm not sure why this would be dependent on multithreading. But
    /// if this is indeed the issue, then a double roundtrip fixes it.
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

impl Drop for State {
    /// Disconnect the clients and let the compositor reap them, before the display goes away.
    ///
    /// A wayland resource handle holds a strong `Arc` to its own object data (the `data` field
    /// wayland-scanner puts on every generated `Resource`), and smithay's
    /// `PrivateSurfaceData::init` pushes `surface.clone()` into the surface's *own* `children`
    /// list. That is a self-cycle, and the only thing that breaks it is `cleanup()`, which runs
    /// from `ObjectData::destroyed`. Tearing the display down cold never calls `destroyed` —
    /// `wl_display_destroy_clients` runs with wayland-backend's `PENDING_DESTRUCTORS` set, so
    /// destructors are only *queued* — and every surface the fixture ever made stays alive, with
    /// its caches, hooks and role state, for the rest of the process. Memory only: no descriptor
    /// is involved. But the suite builds ~1800 of these, which is what makes it worth an ordered
    /// teardown instead of a shrug.
    ///
    /// Dropping the clients closes their sockets; the compositor only notices when it dispatches,
    /// hence the pump. That is the same path a client exiting mid-test takes, so nothing here is a
    /// teardown-only shortcut.
    fn drop(&mut self) {
        // A test that is already failing gets no teardown: a panic raised inside `dispatch` while
        // this one unwinds is an abort, and it would take the whole binary down instead of one
        // test. Leaking on the way out of a failure is the cheaper mistake.
        if std::thread::panicking() {
            return;
        }

        self.clients.clear();

        // Bounded, because a compositor that will not reap is a bug to see, not to hang on. One
        // dispatch is enough in practice; the rest are for a client whose disconnect races it.
        for _ in 0..5 {
            self.server.dispatch();
        }
    }
}
