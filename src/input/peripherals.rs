//! `org.gnome.desktop.peripherals` — the pointer and keyboard device settings.
//!
//! GNOME's way replaces niri's: these used to come from the config file's `input {}` block, and
//! now come from the same schemas GNOME Settings writes, so the Settings → Mouse & Touchpad panel
//! is the way to configure a device. Reference is mutter's
//! `src/backends/meta-input-settings.c` (the `update_*` functions) together with its libinput
//! backend `src/backends/native/meta-input-settings-native.c`, which is where each GSettings enum
//! is turned into a libinput value.
//!
//! The output is deliberately the existing [`niri_config`] device structs rather than a new model:
//! [`super::apply_libinput_settings`] already knows how to push those onto a device, and the rest
//! of the compositor (scroll factors, the repeat timer) reads them where it always did.
//!
//! Known gaps, all of them a setting GNOME has and libinput-as-we-drive-it does not express:
//! `click-method='none'` (the `input` crate's `ClickMethod` has no `None`), the touchpad's
//! `disable-while-typing-timeout`, and the mouse's `double-click`/`drag-threshold`, which are
//! toolkit-level in GNOME and never reach the compositor.

use gio::prelude::SettingsExt;
use niri_config::input::{
    AccelProfile, ClickMethod, Mouse, TapButtonMap, Touchpad, Trackball, Trackpoint,
};
use niri_config::ScrollMethod;

/// The device settings read from `org.gnome.desktop.peripherals.*`.
///
/// [`Default`] is what the compositor runs on with no schemas installed: the `niri_config`
/// defaults, which are themselves GNOME's schema defaults (see `niri-config/src/input.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct Peripherals {
    pub touchpad: Touchpad,
    pub mouse: Mouse,
    /// `org.gnome.desktop.peripherals.pointingstick` — GNOME's name for what niri calls a
    /// trackpoint. Same device: mutter keys both off `ID_INPUT_POINTINGSTICK`.
    pub trackpoint: Trackpoint,
    pub trackball: Trackball,
    /// Milliseconds before a held key starts repeating (`keyboard delay`).
    pub repeat_delay: u16,
    /// Repeats per second. `keyboard repeat = false` arrives here as **0**, which is how both
    /// the wl_keyboard protocol and our own repeat timer spell "no repeat" — GNOME's separate
    /// on/off boolean has no separate representation downstream.
    pub repeat_rate: u8,
    pub numlock: bool,
}

impl Default for Peripherals {
    fn default() -> Self {
        // Not `#[derive(Default)]`: a zero `repeat_rate` means *no key repeat at all*, which
        // is a very quiet way for a box with no GNOME schemas to lose key repeat entirely.
        let keyboard = niri_config::input::Keyboard::default();
        Self {
            touchpad: Touchpad::default(),
            mouse: Mouse::default(),
            trackpoint: Trackpoint::default(),
            trackball: Trackball::default(),
            repeat_delay: keyboard.repeat_delay,
            repeat_rate: keyboard.repeat_rate,
            numlock: keyboard.numlock,
        }
    }
}

impl Peripherals {
    /// Overlay the live values of whichever of the five schemas are installed.
    pub fn load(
        touchpad: Option<&gio::Settings>,
        mouse: Option<&gio::Settings>,
        keyboard: Option<&gio::Settings>,
        trackball: Option<&gio::Settings>,
        pointingstick: Option<&gio::Settings>,
    ) -> Self {
        let mut this = Self::default();

        // Read the mouse first: the touchpad's `left-handed` has a `mouse` value that defers to
        // it (`update_touchpad_left_handed`, meta-input-settings.c:298-313).
        if let Some(mouse) = mouse {
            this.load_mouse(mouse);
        }
        if let Some(touchpad) = touchpad {
            this.load_touchpad(touchpad);
        }
        if let Some(keyboard) = keyboard {
            this.load_keyboard(keyboard);
        }
        if let Some(trackball) = trackball {
            this.load_trackball(trackball);
        }
        if let Some(pointingstick) = pointingstick {
            this.load_pointingstick(pointingstick);
        }

        this
    }

    fn load_mouse(&mut self, s: &gio::Settings) {
        let c = &mut self.mouse;
        c.left_handed = s.boolean("left-handed");
        c.natural_scroll = s.boolean("natural-scroll");
        c.middle_emulation = s.boolean("middle-click-emulation");
        c.accel_speed = niri_config::FloatOrInt(s.double("speed"));
        c.accel_profile = accel_profile(s);
    }

    fn load_touchpad(&mut self, s: &gio::Settings) {
        let c = &mut self.touchpad;

        // `send-events` is one enum where we have two booleans.
        let (off, disabled_on_external_mouse) = match s.string("send-events").as_str() {
            "disabled" => (true, false),
            "disabled-on-external-mouse" => (false, true),
            "enabled" => (false, false),
            other => {
                warn!("ignoring unrecognized touchpad send-events {other:?}");
                (false, false)
            }
        };
        c.off = off;
        c.disabled_on_external_mouse = disabled_on_external_mouse;

        c.tap = s.boolean("tap-to-click");
        c.drag = Some(s.boolean("tap-and-drag"));
        c.drag_lock = s.boolean("tap-and-drag-lock");
        c.dwt = s.boolean("disable-while-typing");
        c.natural_scroll = s.boolean("natural-scroll");
        c.middle_emulation = s.boolean("middle-click-emulation");
        c.accel_speed = niri_config::FloatOrInt(s.double("speed"));
        c.accel_profile = accel_profile(s);

        c.left_handed = match s.string("left-handed").as_str() {
            "left" => true,
            "right" => false,
            // Follow the mouse — hence reading the mouse first.
            "mouse" => self.mouse.left_handed,
            other => {
                warn!("ignoring unrecognized touchpad left-handed {other:?}");
                false
            }
        };

        c.tap_button_map = match s.string("tap-button-map").as_str() {
            "default" => None,
            "lrm" => Some(TapButtonMap::LeftRightMiddle),
            "lmr" => Some(TapButtonMap::LeftMiddleRight),
            other => {
                warn!("ignoring unrecognized tap-button-map {other:?}");
                None
            }
        };

        c.click_method = match s.string("click-method").as_str() {
            "default" => None,
            "areas" => Some(ClickMethod::ButtonAreas),
            "fingers" => Some(ClickMethod::Clickfinger),
            // libinput has CLICK_METHOD_NONE, but the `input` crate's enum does not, so we
            // cannot ask for it. Leaving it at the device default is the closest thing.
            "none" => {
                warn!("touchpad click-method='none' is not supported; using the device default");
                None
            }
            other => {
                warn!("ignoring unrecognized click-method {other:?}");
                None
            }
        };

        // GNOME models scrolling as two independent booleans and lets libinput arbitrate;
        // we have one enum, so collapse them the way mutter's `update_touchpad_edge_scroll`
        // does (:806-808): two-finger wins when both are on.
        //
        // Divergence, and it is in the collapse rather than the policy: mutter also checks that
        // the device *has* two-finger scrolling before preferring it, per device. This model is
        // built once for all devices, so a two-finger-less touchpad with both keys on gets
        // TwoFinger asked for, libinput refuses it, and the device keeps its default method.
        let edge = s.boolean("edge-scrolling-enabled");
        let two_finger = s.boolean("two-finger-scrolling-enabled");
        c.scroll_method = Some(match (two_finger, edge) {
            (true, _) => ScrollMethod::TwoFinger,
            (false, true) => ScrollMethod::Edge,
            (false, false) => ScrollMethod::NoScroll,
        });
    }

    fn load_keyboard(&mut self, s: &gio::Settings) {
        self.repeat_delay = u16::try_from(s.uint("delay")).unwrap_or(u16::MAX);

        // GNOME stores the *interval* between repeats in ms; we store a rate in Hz.
        let interval = s.uint("repeat-interval");
        self.repeat_rate = if !s.boolean("repeat") || interval == 0 {
            0
        } else {
            u8::try_from(1000 / interval).unwrap_or(u8::MAX)
        };

        // `numlock-state` is only meaningful when GNOME is asked to remember it; otherwise
        // gsd leaves the lock alone and the key holds a stale value (`gsd-keyboard-manager.c`).
        self.numlock = s.boolean("remember-numlock-state") && s.boolean("numlock-state");
    }

    fn load_trackball(&mut self, s: &gio::Settings) {
        let c = &mut self.trackball;
        c.middle_emulation = s.boolean("middle-click-emulation");
        c.accel_profile = accel_profile(s);

        // `update_trackball_scroll_button` (:961) + the libinput backend (:379-390): button 0
        // means no scroll emulation at all, anything else means scroll-on-button-down.
        let button = s.int("scroll-wheel-emulation-button");
        if button > 0 {
            c.scroll_method = Some(ScrollMethod::OnButtonDown);
            c.scroll_button = Some(button_to_evdev(u32::try_from(button).unwrap_or(0)));
            c.scroll_button_lock = s.boolean("scroll-wheel-emulation-button-lock");
        } else {
            c.scroll_method = Some(ScrollMethod::NoScroll);
            c.scroll_button = None;
            c.scroll_button_lock = false;
        }
    }

    fn load_pointingstick(&mut self, s: &gio::Settings) {
        let c = &mut self.trackpoint;
        c.accel_speed = niri_config::FloatOrInt(s.double("speed"));
        c.accel_profile = accel_profile(s);
        c.scroll_method = match s.string("scroll-method").as_str() {
            "default" => None,
            "none" => Some(ScrollMethod::NoScroll),
            "on-button-down" => Some(ScrollMethod::OnButtonDown),
            other => {
                warn!("ignoring unrecognized pointingstick scroll-method {other:?}");
                None
            }
        };
    }
}

/// The `accel-profile` enum, shared by all four pointer schemas.
fn accel_profile(s: &gio::Settings) -> Option<AccelProfile> {
    match s.string("accel-profile").as_str() {
        // "Let the device decide" — `libinput_device_config_accel_get_default_profile`.
        "default" => None,
        "flat" => Some(AccelProfile::Flat),
        "adaptive" => Some(AccelProfile::Adaptive),
        other => {
            warn!("ignoring unrecognized accel-profile {other:?}");
            None
        }
    }
}

/// GNOME stores the scroll button as an **X11 button number**; libinput wants an evdev code.
///
/// `meta_clutter_button_to_evdev` (mutter `src/backends/native/meta-input-device-native.c`):
/// 1/2/3 are the left/middle/right cluster, 8/9 are back/forward, and anything else is offset
/// from `BTN_SIDE`.
fn button_to_evdev(button: u32) -> u32 {
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    const BTN_BACK: u32 = 0x116;
    const BTN_FORWARD: u32 = 0x115;

    match button {
        1 => BTN_LEFT,
        2 => BTN_MIDDLE,
        3 => BTN_RIGHT,
        8 => BTN_BACK,
        9 => BTN_FORWARD,
        other => BTN_SIDE + other - 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A memory-backed store per test: the real dconf would be the developer's own session.
    fn store(schema: &str) -> gio::Settings {
        let backend = gio::memory_settings_backend_new();
        gio::Settings::with_backend(schema, &backend)
    }

    /// With no schemas at all the model is the compiled-in defaults, which is what a session
    /// outside GNOME runs on — and they are GNOME's own schema defaults.
    #[test]
    fn no_schemas_leaves_the_defaults() {
        let p = Peripherals::load(None, None, None, None, None);
        assert_eq!(p, Peripherals::default());
        assert!(p.touchpad.tap);
        assert!(p.touchpad.natural_scroll);
    }

    /// The load of five *untouched* stores must land on exactly the compiled-in defaults.
    ///
    /// This is the guard that makes the fallback honest rather than decorative: without a GNOME
    /// installation the compositor runs on `Peripherals::default()`, and it should behave like a
    /// GNOME session nobody has configured — not like whatever niri happened to default to.
    /// It also catches a mapping that reads the wrong key, since a pristine store returns the
    /// schema default for every key we ask for.
    #[test]
    fn peripherals_defaults_match_a_pristine_gnome_store() {
        let loaded = Peripherals::load(
            Some(&store("org.gnome.desktop.peripherals.touchpad")),
            Some(&store("org.gnome.desktop.peripherals.mouse")),
            Some(&store("org.gnome.desktop.peripherals.keyboard")),
            Some(&store("org.gnome.desktop.peripherals.trackball")),
            Some(&store("org.gnome.desktop.peripherals.pointingstick")),
        );
        assert_eq!(loaded, Peripherals::default());
    }

    #[test]
    fn touchpad_send_events_splits_into_two_flags() {
        let s = store("org.gnome.desktop.peripherals.touchpad");
        let mut p = Peripherals::default();

        p.load_touchpad(&s);
        assert!(!p.touchpad.off);
        assert!(!p.touchpad.disabled_on_external_mouse);

        s.set_string("send-events", "disabled-on-external-mouse")
            .unwrap();
        p.load_touchpad(&s);
        assert!(!p.touchpad.off);
        assert!(p.touchpad.disabled_on_external_mouse);

        s.set_string("send-events", "disabled").unwrap();
        p.load_touchpad(&s);
        assert!(p.touchpad.off);
        assert!(!p.touchpad.disabled_on_external_mouse);
    }

    /// The touchpad's `mouse` handedness is not a third state — it reads the *mouse* key, which
    /// is why the mouse has to be loaded first.
    #[test]
    fn touchpad_handedness_can_follow_the_mouse() {
        let touchpad = store("org.gnome.desktop.peripherals.touchpad");
        let mouse = store("org.gnome.desktop.peripherals.mouse");
        mouse.set_boolean("left-handed", true).unwrap();

        // The schema default is already "mouse".
        let p = Peripherals::load(Some(&touchpad), Some(&mouse), None, None, None);
        assert!(p.mouse.left_handed);
        assert!(p.touchpad.left_handed);

        // An explicit value wins over the mouse's.
        touchpad.set_string("left-handed", "right").unwrap();
        let p = Peripherals::load(Some(&touchpad), Some(&mouse), None, None, None);
        assert!(p.mouse.left_handed);
        assert!(!p.touchpad.left_handed);
    }

    /// Two booleans collapse into our one enum, two-finger preferred (mutter's rule).
    #[test]
    fn touchpad_scrolling_booleans_collapse_to_a_method() {
        let s = store("org.gnome.desktop.peripherals.touchpad");
        let mut p = Peripherals::default();

        p.load_touchpad(&s);
        assert_eq!(p.touchpad.scroll_method, Some(ScrollMethod::TwoFinger));

        s.set_boolean("edge-scrolling-enabled", true).unwrap();
        p.load_touchpad(&s);
        assert_eq!(
            p.touchpad.scroll_method,
            Some(ScrollMethod::TwoFinger),
            "two-finger must win when both are on"
        );

        s.set_boolean("two-finger-scrolling-enabled", false)
            .unwrap();
        p.load_touchpad(&s);
        assert_eq!(p.touchpad.scroll_method, Some(ScrollMethod::Edge));

        s.set_boolean("edge-scrolling-enabled", false).unwrap();
        p.load_touchpad(&s);
        assert_eq!(p.touchpad.scroll_method, Some(ScrollMethod::NoScroll));
    }

    /// An interval in milliseconds becomes a rate in hertz, and `repeat = false` becomes rate 0
    /// — the only spelling of "no repeat" anything downstream understands.
    #[test]
    fn keyboard_repeat_interval_becomes_a_rate() {
        let s = store("org.gnome.desktop.peripherals.keyboard");
        let mut p = Peripherals::default();

        // Schema defaults: repeat on, delay 500ms, interval 30ms.
        p.load_keyboard(&s);
        assert_eq!(p.repeat_delay, 500);
        assert_eq!(p.repeat_rate, 33);

        s.set_uint("repeat-interval", 100).unwrap();
        p.load_keyboard(&s);
        assert_eq!(p.repeat_rate, 10);

        s.set_boolean("repeat", false).unwrap();
        p.load_keyboard(&s);
        assert_eq!(p.repeat_rate, 0);
    }

    /// `numlock-state` only counts when GNOME was asked to remember it; otherwise the key holds
    /// whatever it held last and nothing acts on it.
    #[test]
    fn numlock_state_needs_remembering_to_count() {
        let s = store("org.gnome.desktop.peripherals.keyboard");
        let mut p = Peripherals::default();

        s.set_boolean("numlock-state", true).unwrap();
        p.load_keyboard(&s);
        assert!(p.numlock);

        s.set_boolean("remember-numlock-state", false).unwrap();
        p.load_keyboard(&s);
        assert!(!p.numlock);
    }

    /// Button 0 means "no scroll emulation"; anything else turns into an evdev code, not the
    /// X11 button number GNOME stores.
    #[test]
    fn trackball_scroll_button_zero_disables_scrolling() {
        let s = store("org.gnome.desktop.peripherals.trackball");
        let mut p = Peripherals::default();

        p.load_trackball(&s);
        assert_eq!(p.trackball.scroll_method, Some(ScrollMethod::NoScroll));
        assert_eq!(p.trackball.scroll_button, None);

        s.set_int("scroll-wheel-emulation-button", 2).unwrap();
        s.set_boolean("scroll-wheel-emulation-button-lock", true)
            .unwrap();
        p.load_trackball(&s);
        assert_eq!(p.trackball.scroll_method, Some(ScrollMethod::OnButtonDown));
        assert_eq!(p.trackball.scroll_button, Some(0x112)); // BTN_MIDDLE
        assert!(p.trackball.scroll_button_lock);
    }

    /// `default` on any of the four pointer schemas means "leave it to the device", which is
    /// `None` here — not a made-up profile.
    #[test]
    fn accel_profile_default_is_none() {
        let s = store("org.gnome.desktop.peripherals.mouse");
        assert_eq!(accel_profile(&s), None);

        s.set_string("accel-profile", "flat").unwrap();
        assert_eq!(accel_profile(&s), Some(AccelProfile::Flat));

        s.set_string("accel-profile", "adaptive").unwrap();
        assert_eq!(accel_profile(&s), Some(AccelProfile::Adaptive));
    }
}
