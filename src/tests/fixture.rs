// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

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

        let state = State {
            server: Server::new(config),
            clients: Vec::new(),
        };

        Self {
            state,
            next_input_time: 0,
        }
    }

    /// Run one turn of the compositor's event loop, and the test clients' loops with it.
    ///
    /// **This is the harness's only way to advance the compositor**, and it is
    /// [`Server::turn`] — the body `EventLoop::run` executes, which is what `main.rs` runs. The
    /// clients are pumped first so that whatever they sent last turn is on the socket before the
    /// compositor dispatches.
    ///
    /// A turn used to be two separately-orderable halves (`dispatch`, then `refresh`), and the
    /// compositor's loop was a `Generic` source *inside* the fixture's own loop. That put the turn
    /// boundary wherever the compositor's poll fd happened to become readable — a full hidden turn
    /// mid-`roundtrip`, none at all for a timer, since calloop keeps timers in the loop rather
    /// than on an fd. An animation therefore rendered a single frame and stalled, and
    /// `double_roundtrip` exists because a configure sometimes needed a second, unpredictable
    /// turn to land.
    pub fn turn(&mut self) {
        for client in &mut self.state.clients {
            client.dispatch();
        }
        self.state.server.turn();
    }

    /// Step the clock forward one frame at a time, taking a turn each step, until `pred` holds —
    /// giving up after `within` of *clock* time. Returns whether the predicate came true.
    ///
    /// This is how a test reaches a timer: every deadline in the compositor is on the clock the
    /// test is driving here (see [`crate::utils::timers`]), so a key repeat, a drag countdown or
    /// an idle deadline all come due in the turn that steps past them. It spends no wall-clock,
    /// which is what the wall-clock `dispatch_until` it replaced did — a 3 s repeat test really
    /// slept for up to 3 s.
    /// Pump the loop until `pred` holds, spending **real** wall-clock — for the one deadline
    /// [`advance_until`](Self::advance_until) cannot reach.
    ///
    /// Every deadline the compositor reasons about is on the clock a test drives, so this is not
    /// the tool for one: reach for `advance_until`. What is left on calloop's clock is the
    /// estimated-vblank pacer, which stands in for display hardware — and an output already parked
    /// on it when a test freezes the clock is freed only by real time passing.
    pub fn dispatch_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&mut crate::synoik::State) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            self.turn();
            if pred(&mut self.state.server.state) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn advance_until(
        &mut self,
        within: Duration,
        mut pred: impl FnMut(&mut crate::synoik::State) -> bool,
    ) -> bool {
        const FRAME: Duration = Duration::from_micros(16_667);

        let was_frozen = self.synoik().clock.is_frozen();
        self.freeze_clock();

        let mut elapsed = Duration::ZERO;
        let mut held = pred(&mut self.state.server.state);
        while !held && elapsed < within {
            self.advance_clock(FRAME);
            self.turn();
            elapsed += FRAME;
            held = pred(&mut self.state.server.state);
        }

        if !was_frozen {
            self.synoik().clock.unfreeze();
        }
        held
    }

    pub fn synoik_state(&mut self) -> &mut crate::synoik::State {
        &mut self.state.server.state
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

    /// The output on connector `headless-{n}`, by name rather than by position: a test that
    /// unplugs one would otherwise silently start addressing a different display.
    pub fn synoik_output(&self, n: u8) -> Output {
        let synoik = &self.state.server.state.synoik;
        let connector = format!("headless-{n}");
        let output = synoik
            .global_space
            .outputs()
            .find(|output| output.name() == connector)
            .unwrap_or_else(|| panic!("{connector} is plugged in"));
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
    /// The primitive beside it does only a *part* of a frame:
    /// [`synoik_complete_animations`](Self::synoik_complete_animations) teleports the animation
    /// clock to the end without a turn. A test that needs a transition to *finish the way it
    /// finishes in the session* had to hand-assemble the sequence, and every test assembled it
    /// slightly differently — which is how a behaviour that lands on the frame after an animation
    /// ends can be invisible to the corpus.
    ///
    /// This is that sequence, in the order the real loop runs it: advance the clock by one refresh
    /// interval, pump the event loop, reconcile. One turn draws one frame — the estimated-vblank
    /// pacer stands down for a frozen clock and lets this step be the vblank — so a transition
    /// settled here has been *rendered* frame by frame, and the last frame drawn is the settled
    /// one. It gives up after `max_frames` so a transition that never settles fails the test
    /// instead of hanging it.
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
            self.turn();
            if !self.transitions_ongoing() {
                // One more full frame: the live loop reconciles *after* the last animated frame,
                // and that trailing pass is where anything deferred to the end of a transition
                // actually lands.
                self.advance_clock(FRAME);
                self.turn();
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
        if !self.run_until_settled(600) {
            let outputs: Vec<_> = self.synoik().global_space.outputs().cloned().collect();
            let causes: Vec<_> = outputs
                .iter()
                .map(|output| (output.name(), self.synoik().anim_causes(output)))
                .filter(|(_, causes)| {
                    !causes
                        .difference(crate::frame_log::AnimCauses::ONGOING)
                        .is_empty()
                })
                .collect();
            panic!("still animating after 10s of frames: {causes:?}");
        }
    }

    /// Record the element list of every frame the compositor draws from here on, per output.
    ///
    /// This is the way to ask "what did the compositor put on screen". The alternative a test
    /// reaches for otherwise — call `update_render_elements`, then `render_to_vec` — runs the
    /// render path a *second* time, from the test, and the resulting list is a product of the
    /// test's own sequencing. This one is the list the frame was drawn from, taken inside
    /// `Headless::render` on the frames the compositor's own redraw machinery chose to run.
    ///
    /// The recording keeps growing for the rest of the test: it installs the one frame sink, so
    /// taking a second recording replaces the first, and a test that wants a fresh window calls
    /// [`FrameRecording::clear`] rather than recording again.
    /// Needs a renderer: the element pass runs inside `Headless::render_element_states`, which has
    /// nothing to build a `RenderCtx` from without one and returns before the sink is called. A
    /// test that records frames therefore calls `add_renderer` and skips itself where there is no
    /// Vulkan device, the same as every other test that needs the real render path.
    pub fn record_frames(&mut self) -> FrameRecording {
        assert!(
            self.synoik_state()
                .backend
                .headless()
                .with_vulkan_renderer(|_| ())
                .is_some(),
            "record_frames without a renderer: no frame is ever drawn, so the recording would \
             stay empty and every assertion over it would be vacuous. Call \
             `backend.headless().add_renderer()` first, and skip the test where that fails."
        );
        let frames: std::rc::Rc<std::cell::RefCell<Vec<crate::frame_log::FrameSnapshot>>> =
            Default::default();
        let sink = frames.clone();
        self.synoik_state().backend.headless().frame_sink =
            Some(Box::new(move |_vk, output, elements| {
                let scale = output.current_scale().fractional_scale();
                let bounds = smithay::utils::Rectangle::from_size(
                    output
                        .current_mode()
                        .map_or_else(Default::default, |m| m.size),
                );
                sink.borrow_mut().push(crate::frame_log::snapshot_frame(
                    &output.name(),
                    elements,
                    scale.into(),
                    bounds,
                ));
            }));
        FrameRecording { frames }
    }

    /// Whether anything is still animating, on any output — **the compositor's own answer**.
    ///
    /// `State::redraw` decides whether to queue another frame from `Synoik::anim_causes`, so the
    /// harness asks the same question rather than keeping a second list. An earlier version asked
    /// only whether a *monitor transition* was running, which is a much smaller set: it stopped two
    /// frames into a switch and left timed overlays — an OSD, a banner — still up, so a settle that
    /// looked faithful was not.
    pub fn transitions_ongoing(&mut self) -> bool {
        let outputs: Vec<_> = self.synoik().global_space.outputs().cloned().collect();
        outputs.iter().any(|output| {
            !self
                .synoik()
                .anim_causes(output)
                .difference(crate::frame_log::AnimCauses::ONGOING)
                .is_empty()
        })
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

    /// Run the compositor for `duration` of its own time, one frame at a time, the way
    /// [`run_until_settled`](Self::run_until_settled) does — for a gesture that is *held* rather
    /// than one that finishes.
    ///
    /// A hold cannot be driven by settling: nothing is animating while a key is merely down, so
    /// `settle` returns after a frame or two and the threshold never passes. Nor by teleporting
    /// the clock: the deadline has to be crossed by frames, since the frame is what notices it.
    ///
    /// **Leaves the clock frozen**, unlike [`run_until_settled`](Self::run_until_settled), which
    /// restores it. That helper can: by the time it returns nothing is animating, so the rewind
    /// that unfreezing performs — back from the pinned time to real monotonic time — lands on
    /// animations that are already done. A hold ends with transitions *in flight*, and rewinding
    /// under them puts their start time in the future: an animation mid-slide reads as 0 again and
    /// the thing under test flicks back to where it started.
    pub fn run_frames_for(&mut self, duration: Duration) {
        const FRAME: Duration = Duration::from_micros(16_667);

        self.freeze_clock();
        let mut elapsed = Duration::ZERO;
        while elapsed < duration {
            self.advance_clock(FRAME);
            self.turn();
            elapsed += FRAME;
        }
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

    /// Plug a display in on connector `headless-{n}` whose EDID carries `serial`, so a test can
    /// put a *different* panel on a connector another one used.
    pub fn add_output_with_serial(&mut self, n: u8, size: (u16, u16), serial: &str) {
        let state = self.synoik_state();
        let synoik = &mut state.synoik;
        state
            .backend
            .headless()
            .add_output_with_serial(synoik, n, size, serial);
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

        // No source, no second loop: every client's loop is dispatched by `Fixture::turn`, which
        // is the only thing that advances anything here.
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
            self.turn();
        }
    }

    /// Roundtrip twice in a row — **required whenever the roundtrip is meant to deliver a
    /// configure**.
    ///
    /// A configure is sent from the turn's *callback* (`refresh_and_flush_clients`), which runs
    /// after the dispatch that answered the sync — so the `done` the first roundtrip waits on is
    /// already on its way out when the configure is queued behind it, and the client's dispatch
    /// stops at `done`. The second roundtrip's first turn delivers it.
    ///
    /// Not a flake tolerance: with one roundtrip, 13 tests fail deterministically, on a missing
    /// configure or on `wrong configure serial`.
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
        // turn is enough in practice; the rest are for a client whose disconnect races it.
        for _ in 0..5 {
            self.server.turn();
        }
    }
}

/// The pixels the **screen** holds for `output`: the swapchain slot as damage tracking left it.
///
/// This is the only arm that can show a missing repaint. Every capture path — screenshot,
/// screencast, `render_to_vec` — goes through `render_helpers::render_elements`, which clears a
/// fresh target and draws every element over its whole geometry with no damage tracker and no
/// occlusion culling. A pixel nobody was told to repaint is therefore repainted by construction
/// in a capture, and a bug of that class is invisible to one however the picture is compared.
///
/// Pair it with [`capture_pixels`] and [`never_painted`].
pub fn screen_pixels(f: &mut Fixture, output: &Output) -> (Vec<u8>, i32, i32) {
    f.synoik_state()
        .backend
        .headless()
        .last_frame_pixels(output)
        .expect(
            "no frame has been drawn for this output, so there is no screen to read — the \
             comparison below would be against an empty slot",
        )
}

/// The same frame through the **capture** path: one full-damage draw into a fresh target.
///
/// Deliberately a second render, and deliberately the path a screenshot takes — it is the control
/// arm, and what it is worth is that it cannot carry a stale pixel. See [`screen_pixels`].
pub fn capture_pixels(f: &mut Fixture, output: &Output) -> (Vec<u8>, i32, i32) {
    use smithay::backend::allocator::Fourcc;
    use smithay::utils::{Physical, Scale, Size, Transform};

    use crate::render_helpers::{RenderCtx, RenderTarget};

    let size: Size<i32, Physical> = output.current_mode().expect("the output has a mode").size;
    let state = f.synoik_state();
    let pixels = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<Vec<u8>> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(output));
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, output, true);
            crate::render_helpers::render_to_vec(
                vk,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )
        })
        .expect("the fixture must hold a Vulkan renderer")
        .expect("compositing through Vulkan must not error");
    (pixels, size.w, size.h)
}

/// Where the screen disagrees with a full redraw: the count of differing pixels and the box that
/// contains them, or `None` when the two agree.
///
/// The box is what makes a failure actionable — it says *where* the screen went stale rather than
/// only that it did. One channel step of slack, because the two arms take different render passes
/// to the same pixels and a rounding difference is not a missing repaint.
pub fn never_painted(
    screen: &[u8],
    capture: &[u8],
    w: i32,
    h: i32,
) -> Option<(u64, Rectangle<i32, smithay::utils::Physical>)> {
    use smithay::utils::{Point, Size};

    assert_eq!(
        screen.len(),
        capture.len(),
        "the screen and the capture must be the same frame"
    );

    let mut differing = 0u64;
    let mut bbox: Option<Rectangle<i32, smithay::utils::Physical>> = None;
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            if !(0..4).any(|c| screen[i + c].abs_diff(capture[i + c]) > 1) {
                continue;
            }
            differing += 1;
            let px = Rectangle::new(Point::from((x, y)), Size::from((1, 1)));
            bbox = Some(bbox.map_or(px, |b| {
                let x0 = b.loc.x.min(x);
                let y0 = b.loc.y.min(y);
                let x1 = (b.loc.x + b.size.w).max(x + 1);
                let y1 = (b.loc.y + b.size.h).max(y + 1);
                Rectangle::new(Point::from((x0, y0)), Size::from((x1 - x0, y1 - y0)))
            }));
        }
    }
    (differing > 0).then(|| (differing, bbox.unwrap_or_default()))
}

/// The frames the compositor drew while a recording was installed. See
/// [`Fixture::record_frames`].
#[derive(Clone)]
pub struct FrameRecording {
    frames: std::rc::Rc<std::cell::RefCell<Vec<crate::frame_log::FrameSnapshot>>>,
}

impl FrameRecording {
    /// The last frame drawn for `output`.
    ///
    /// Panics when there is none, rather than returning an `Option` a test can quietly let fall
    /// through: a recording with no frames in it makes every assertion below it vacuous, and that
    /// is the failure mode this whole port exists to remove.
    pub fn last(&self, output: &Output) -> crate::frame_log::FrameSnapshot {
        let name = output.name();
        self.frames
            .borrow()
            .iter()
            .rev()
            .find(|f| f.output == name)
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "no frame was drawn for {name} while recording — every assertion about this \
                     frame would pass for want of a frame. Drawn: {:?}",
                    self.outputs()
                )
            })
    }

    /// Which outputs drew, and how many frames each — the diagnostic for an empty recording.
    pub fn outputs(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for frame in self.frames.borrow().iter() {
            match counts.iter_mut().find(|(name, _)| *name == frame.output) {
                Some((_, n)) => *n += 1,
                None => counts.push((frame.output.clone(), 1)),
            }
        }
        counts
    }
}
