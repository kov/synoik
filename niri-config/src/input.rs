use smithay::input::keyboard::XkbConfig;
use smithay::reexports::input;

use crate::binds::Modifiers;
use crate::utils::{Flag, MergeWith, Percent};
use crate::FloatOrInt;

#[derive(Debug, Default, PartialEq)]
pub struct Input {
    pub keyboard: Keyboard,
    pub touchpad: Touchpad,
    pub mouse: Mouse,
    pub trackpoint: Trackpoint,
    pub trackball: Trackball,
    pub tablet: Tablet,
    pub touch: Touch,
    pub disable_power_key_handling: bool,
    pub warp_mouse_to_focus: Option<WarpMouseToFocus>,
    pub focus_follows_mouse: Option<FocusFollowsMouse>,
    pub workspace_auto_back_and_forth: bool,
    pub mod_key: Option<ModKey>,
    pub mod_key_nested: Option<ModKey>,
}

#[derive(Debug, Default, PartialEq)]
pub struct InputPart {
    pub keyboard: Option<KeyboardPart>,
    pub touchpad: Option<Touchpad>,
    pub mouse: Option<Mouse>,
    pub trackpoint: Option<Trackpoint>,
    pub trackball: Option<Trackball>,
    pub tablet: Option<Tablet>,
    pub touch: Option<Touch>,
    pub disable_power_key_handling: Option<Flag>,
    pub warp_mouse_to_focus: Option<WarpMouseToFocus>,
    pub focus_follows_mouse: Option<FocusFollowsMouse>,
    pub workspace_auto_back_and_forth: Option<Flag>,
    pub mod_key: Option<ModKey>,
    pub mod_key_nested: Option<ModKey>,
}

impl MergeWith<InputPart> for Input {
    fn merge_with(&mut self, part: &InputPart) {
        merge!(
            (self, part),
            keyboard,
            disable_power_key_handling,
            workspace_auto_back_and_forth,
        );

        merge_clone!(
            (self, part),
            touchpad,
            mouse,
            trackpoint,
            trackball,
            tablet,
            touch,
        );

        merge_clone_opt!(
            (self, part),
            warp_mouse_to_focus,
            focus_follows_mouse,
            mod_key,
            mod_key_nested,
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Keyboard {
    pub xkb: Xkb,
    pub repeat_delay: u16,
    pub repeat_rate: u8,
    pub track_layout: TrackLayout,
    pub numlock: bool,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            xkb: Default::default(),
            // GNOME's defaults, from org.gnome.desktop.peripherals.keyboard: `delay` is
            // 500ms and `repeat-interval` is 30ms, which is 33 repeats a second. (niri's
            // were 600/25, chosen to match wlroots and sway — niri's way, so it goes.)
            repeat_delay: 500,
            repeat_rate: 33,
            track_layout: Default::default(),
            // Off: `org.gnome.desktop.peripherals.keyboard numlock-state` defaults to
            // false, so a GNOME session nobody has configured comes up with numlock off.
            // The config file we used to ship turned it on; that was niri's default, not
            // GNOME's, and GNOME's wins.
            numlock: false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct KeyboardPart {
    pub xkb: Option<Xkb>,
    pub repeat_delay: Option<u16>,
    pub repeat_rate: Option<u8>,
    pub track_layout: Option<TrackLayout>,
    pub numlock: Option<Flag>,
}

impl MergeWith<KeyboardPart> for Keyboard {
    fn merge_with(&mut self, part: &KeyboardPart) {
        merge_clone!((self, part), xkb, repeat_delay, repeat_rate, track_layout);
        merge!((self, part), numlock);
    }
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct Xkb {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
    pub file: Option<String>,
}

impl Xkb {
    pub fn to_xkb_config(&self) -> XkbConfig<'_> {
        XkbConfig {
            rules: &self.rules,
            model: &self.model,
            layout: &self.layout,
            variant: &self.variant,
            options: self.options.clone(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TrackLayout {
    /// The layout change is global.
    #[default]
    Global,
    /// The layout change is window local.
    Window,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ScrollFactor {
    pub base: Option<FloatOrInt<0, 100>>,
    pub horizontal: Option<FloatOrInt<-100, 100>>,
    pub vertical: Option<FloatOrInt<-100, 100>>,
}

impl ScrollFactor {
    pub fn h_v_factors(&self) -> (f64, f64) {
        let base_value = self.base.map(|f| f.0).unwrap_or(1.0);
        let h = self.horizontal.map(|f| f.0).unwrap_or(base_value);
        let v = self.vertical.map(|f| f.0).unwrap_or(base_value);
        (h, v)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Touchpad {
    pub off: bool,
    pub tap: bool,
    pub dwt: bool,
    pub dwtp: bool,
    pub drag: Option<bool>,
    pub drag_lock: bool,
    pub natural_scroll: bool,
    pub click_method: Option<ClickMethod>,
    pub accel_speed: FloatOrInt<-1, 1>,
    pub accel_profile: Option<AccelProfile>,
    pub scroll_method: Option<ScrollMethod>,
    pub scroll_button: Option<u32>,
    pub scroll_button_lock: bool,
    pub tap_button_map: Option<TapButtonMap>,
    pub left_handed: bool,
    pub disabled_on_external_mouse: bool,
    pub middle_emulation: bool,
    pub scroll_factor: Option<ScrollFactor>,
}

impl Default for Touchpad {
    fn default() -> Self {
        Self {
            off: false,
            // Every value here is `org.gnome.desktop.peripherals.touchpad`'s schema default,
            // so a box with no GNOME schemas installed behaves like one that has them
            // untouched. `peripherals_defaults_match_a_pristine_gnome_store` pins that.
            tap: true,
            natural_scroll: true,
            dwt: true,
            // GNOME has no dwtp key.
            dwtp: false,
            drag: Some(true),
            drag_lock: false,
            click_method: Some(ClickMethod::Clickfinger),
            accel_speed: Default::default(),
            accel_profile: None,
            scroll_method: Some(ScrollMethod::TwoFinger),
            scroll_button: None,
            scroll_button_lock: false,
            tap_button_map: None,
            left_handed: false,
            disabled_on_external_mouse: false,
            middle_emulation: false,
            scroll_factor: None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Mouse {
    pub off: bool,
    pub natural_scroll: bool,
    pub accel_speed: FloatOrInt<-1, 1>,
    pub accel_profile: Option<AccelProfile>,
    pub scroll_method: Option<ScrollMethod>,
    pub scroll_button: Option<u32>,
    pub scroll_button_lock: bool,
    pub left_handed: bool,
    pub middle_emulation: bool,
    pub scroll_factor: Option<ScrollFactor>,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Trackpoint {
    pub off: bool,
    pub natural_scroll: bool,
    pub accel_speed: FloatOrInt<-1, 1>,
    pub accel_profile: Option<AccelProfile>,
    pub scroll_method: Option<ScrollMethod>,
    pub scroll_button: Option<u32>,
    pub scroll_button_lock: bool,
    pub left_handed: bool,
    pub middle_emulation: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trackball {
    pub off: bool,
    pub natural_scroll: bool,
    pub accel_speed: FloatOrInt<-1, 1>,
    pub accel_profile: Option<AccelProfile>,
    pub scroll_method: Option<ScrollMethod>,
    pub scroll_button: Option<u32>,
    pub scroll_button_lock: bool,
    pub left_handed: bool,
    pub middle_emulation: bool,
}

impl Default for Trackball {
    fn default() -> Self {
        Self {
            off: false,
            natural_scroll: false,
            accel_speed: Default::default(),
            accel_profile: None,
            // `org.gnome.desktop.peripherals.trackball scroll-wheel-emulation-button`
            // defaults to 0, which mutter turns into "no scrolling at all" rather than the
            // device default (meta-input-settings-native.c:379-384).
            scroll_method: Some(ScrollMethod::NoScroll),
            scroll_button: None,
            scroll_button_lock: false,
            left_handed: false,
            middle_emulation: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickMethod {
    Clickfinger,
    ButtonAreas,
}

impl From<ClickMethod> for input::ClickMethod {
    fn from(value: ClickMethod) -> Self {
        match value {
            ClickMethod::Clickfinger => Self::Clickfinger,
            ClickMethod::ButtonAreas => Self::ButtonAreas,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelProfile {
    Adaptive,
    Flat,
}

impl From<AccelProfile> for input::AccelProfile {
    fn from(value: AccelProfile) -> Self {
        match value {
            AccelProfile::Adaptive => Self::Adaptive,
            AccelProfile::Flat => Self::Flat,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollMethod {
    NoScroll,
    TwoFinger,
    Edge,
    OnButtonDown,
}

impl From<ScrollMethod> for input::ScrollMethod {
    fn from(value: ScrollMethod) -> Self {
        match value {
            ScrollMethod::NoScroll => Self::NoScroll,
            ScrollMethod::TwoFinger => Self::TwoFinger,
            ScrollMethod::Edge => Self::Edge,
            ScrollMethod::OnButtonDown => Self::OnButtonDown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapButtonMap {
    LeftRightMiddle,
    LeftMiddleRight,
}

impl From<TapButtonMap> for input::TapButtonMap {
    fn from(value: TapButtonMap) -> Self {
        match value {
            TapButtonMap::LeftRightMiddle => Self::LeftRightMiddle,
            TapButtonMap::LeftMiddleRight => Self::LeftMiddleRight,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Tablet {
    pub off: bool,
    pub calibration_matrix: Option<Vec<f32>>,
    pub map_to_output: Option<String>,
    pub map_to_focused_output: bool,
    pub map_to_focused_window: bool,
    pub left_handed: bool,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Touch {
    pub off: bool,
    pub calibration_matrix: Option<Vec<f32>>,
    pub map_to_output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FocusFollowsMouse {
    pub max_scroll_amount: Option<Percent>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct WarpMouseToFocus {
    pub mode: Option<WarpMouseToFocusMode>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum WarpMouseToFocusMode {
    CenterXy,
    CenterXyAlways,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ModKey {
    Ctrl,
    Shift,
    Alt,
    Super,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

impl ModKey {
    pub fn to_modifiers(&self) -> Modifiers {
        match self {
            ModKey::Ctrl => Modifiers::CTRL,
            ModKey::Shift => Modifiers::SHIFT,
            ModKey::Alt => Modifiers::ALT,
            ModKey::Super => Modifiers::SUPER,
            ModKey::IsoLevel3Shift => Modifiers::ISO_LEVEL3_SHIFT,
            ModKey::IsoLevel5Shift => Modifiers::ISO_LEVEL5_SHIFT,
        }
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;

    use super::*;

    #[test]
    fn scroll_factor_h_v_factors() {
        let sf = ScrollFactor {
            base: Some(FloatOrInt(2.0)),
            horizontal: None,
            vertical: None,
        };
        assert_debug_snapshot!(sf.h_v_factors(), @r#"
        (
            2.0,
            2.0,
        )
        "#);

        let sf = ScrollFactor {
            base: None,
            horizontal: Some(FloatOrInt(3.0)),
            vertical: Some(FloatOrInt(-1.0)),
        };
        assert_debug_snapshot!(sf.h_v_factors(), @r#"
        (
            3.0,
            -1.0,
        )
        "#);

        let sf = ScrollFactor {
            base: Some(FloatOrInt(2.0)),
            horizontal: Some(FloatOrInt(1.0)),
            vertical: None,
        };
        assert_debug_snapshot!(sf.h_v_factors(), @r"
        (
            1.0,
            2.0,
        )
        ");
    }
}
