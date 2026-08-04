// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `BrightnessManager` — the shell-side brightness algebra (`js/misc/brightnessManager.js`).
//!
//! GNOME 50.1 splits brightness in two: mutter owns the hardware (ported in [`crate::backlight`]
//! and the TTY backend), and the shell owns a small algebra of *scales* on top of it. A scale is a
//! 0..1 value with a step count; there is one per backlit monitor plus one global scale, and the
//! global one is what the quick-settings slider drives (`js/ui/status/brightness.js:37-92`).
//!
//! The interesting part is how the two levels stay related. Each monitor keeps a `scaleFactor`
//! relative to the brightest monitor, so dragging the global slider moves every monitor while
//! preserving their ratios, and moving one monitor re-derives the ratios and pulls the global
//! slider to the new maximum. On top of that sits an idle-dimming clamp and an auto-brightness
//! bias. All of it is in `_sync` (`:186-262`), whose three phases run in a fixed order.
//!
//! This is a plain-data port: no signals, no GObject. Where GNOME reacts to a property
//! notification, we take an explicit call and return the hardware writes to perform, so the
//! compositor stays the only thing that talks to the device.
//!
//! Divergences (also in `docs/fork/panel-status-port.md` under Q4):
//! - **D5**: a scale is per *output*, not per logical monitor. These differ only under mirroring,
//!   which we do not support yet; with mirroring, GNOME writes every backlight behind one scale.
//! - **D4**: the keybindings that `_sync` would trigger live in the compositor's action table, not
//!   here. The step/cycle methods are ported here anyway, since they define the scale's arithmetic.
//! - Raw brightness is **rounded** from the scale's fraction, where GJS truncates on its way into
//!   mutter's int property. Rounding is a half-step closer to what the user asked for and keeps
//!   `set(get(x)) == x` stable.

use crate::backlight::{BacklightSnapshot, OutputBacklight};

/// `SCALE_VALUE_N_STEPS` (`brightnessManager.js:8`).
const SCALE_VALUE_N_STEPS: u32 = 20;
/// `SCALE_VALUE_CHANGE_EPSILON` (`:9`).
const SCALE_VALUE_CHANGE_EPSILON: f64 = 0.001;

/// `org.gnome.settings-daemon.plugins.power idle-brightness` (`:28-29`) — still read from the
/// power plugin's schema even though gsd-power no longer owns the backlight. The schema default is
/// 30, i.e. dim to 30%.
pub const DEFAULT_DIMMING_TARGET: f64 = 0.3;

/// One raw hardware write the manager wants performed, for the caller to push through the
/// backlight write serializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklightWrite {
    pub connector: String,
    pub brightness: i32,
}

/// One entry of `_showOSD`'s `osdMonitors` dict (`:264-275`): a monitor, and the scale value to
/// draw on its bar. There is no `max_level` — brightness maxes out at 1.0.
#[derive(Debug, Clone, PartialEq)]
pub struct OsdRequest {
    pub connector: String,
    pub level: f64,
}

/// What one pass of the algebra produced: the hardware writes to push, and the OSDs to show.
///
/// GNOME reaches out from `_sync` to `Main.osdWindowManager` directly (`:227-239,264-275`); we
/// return the request so the compositor stays the only thing that touches either the device or the
/// screen.
#[derive(Debug, Clone, PartialEq, Default)]
#[must_use = "the writes must be pushed to the hardware, and the OSD shown"]
pub struct BrightnessUpdate {
    pub writes: Vec<BacklightWrite>,
    /// The monitors whose OSD to show — empty when nothing moved, or when the caller asked for no
    /// OSD. Non-empty means *only* these monitors show one; `osdWindowManager.show` cancels the
    /// rest (`osdWindow.js:172-182`).
    pub osd: Vec<OsdRequest>,
}

impl BrightnessUpdate {
    /// No hardware write — which is also how a caller detects "no scale moved".
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }
}

/// What the quick-settings surfaces need from the manager, as a plain snapshot they can hold.
///
/// Mirrors what `BrightnessItem._sync` reads (`brightness.js:57-74`): the global scale decides
/// whether the slider exists at all, and the per-monitor scales fill the detail card and gate its
/// arrow (`menuEnabled = scales.length > 1`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrightnessView {
    /// The global scale's value, or `None` when nothing is backlit — which hides the slider.
    pub global: Option<f64>,
    /// One entry per backlit output, in output order.
    pub monitors: Vec<MonitorView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorView {
    pub connector: String,
    /// The monitor's display name, the label above its row in the card.
    pub name: String,
    pub value: f64,
}

/// `BrightnessScale` (`:279-330`): a 0..1 value with a step count.
#[derive(Debug, Clone, PartialEq)]
pub struct BrightnessScale {
    name: String,
    value: f64,
    n_steps: u32,
}

impl BrightnessScale {
    pub fn new(name: impl Into<String>, value: f64, n_steps: u32) -> Self {
        Self {
            name: name.into(),
            value: value.clamp(0., 1.),
            n_steps: n_steps.max(1),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn n_steps(&self) -> u32 {
        self.n_steps
    }

    /// `_setValue` (`:324-327`): always clamped into 0..1.
    pub fn set_value(&mut self, value: f64) {
        self.value = value.clamp(0., 1.);
    }

    /// `stepUp` (`:311-313`).
    pub fn step_up(&mut self) {
        self.set_value(self.value + 1. / f64::from(self.n_steps));
    }

    /// `stepDown` (`:315-317`).
    pub fn step_down(&mut self) {
        self.set_value(self.value - 1. / f64::from(self.n_steps));
    }

    /// `cycleUp` (`:319-322`): steps up, but wraps to 0 once at the top — what the single
    /// brightness-cycle key does.
    pub fn cycle_up(&mut self) {
        if (1. - self.value).abs() < SCALE_VALUE_CHANGE_EPSILON {
            self.set_value(0.);
        } else {
            self.step_up();
        }
    }
}

/// `MonitorBrightnessScale` (`:331-407`): one monitor's scale, plus the bookkeeping that relates
/// it to the global scale and to the hardware.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorScale {
    scale: BrightnessScale,
    /// The output this scale drives (D5: per-output, not per logical monitor).
    connector: String,
    /// This monitor's brightness as a fraction of the brightest monitor's — what makes the global
    /// slider preserve the ratios between monitors (`updateScaleFactor`, `:405-407`).
    scale_factor: f64,
    /// The last raw value we either read from or wrote to the hardware, so an echo of our own
    /// write is not mistaken for someone else moving the backlight (`:381,400`).
    current_backlight_brightness: i32,
    /// GNOME's `_scaleChanged`: this scale was moved by the user since the last `_sync`.
    changed: bool,
}

impl MonitorScale {
    /// The constructor's derivation (`:338-347`): named after the monitor, with a step count
    /// capped both by the hardware's own step count and by `SCALE_VALUE_N_STEPS` — a backlight
    /// with 3 usable steps gets a 3-step slider.
    fn new(backlight: &OutputBacklight, value: f64) -> Self {
        let hw_steps = u32::try_from(backlight.range.max - backlight.range.min).unwrap_or(1);
        let n_steps = hw_steps.min(SCALE_VALUE_N_STEPS);

        Self {
            scale: BrightnessScale::new(backlight.display_name.clone(), value, n_steps),
            connector: backlight.connector.clone(),
            scale_factor: 1.,
            current_backlight_brightness: -1,
            changed: false,
        }
    }

    pub fn connector(&self) -> &str {
        &self.connector
    }

    pub fn name(&self) -> &str {
        self.scale.name()
    }

    pub fn value(&self) -> f64 {
        self.scale.value()
    }

    pub fn n_steps(&self) -> u32 {
        self.scale.n_steps()
    }

    /// `syncWithBacklight` (`:382-391`): adopt the hardware's value when it moved behind our back.
    /// Returns whether it had.
    fn sync_with_backlight(&mut self, backlight: &OutputBacklight) -> bool {
        if backlight.brightness == self.current_backlight_brightness {
            return false;
        }
        self.current_backlight_brightness = backlight.brightness;
        self.scale.set_value(backlight.relative_brightness());
        true
    }

    /// `syncWithScale` (`:393-395`).
    fn sync_with_scale(&mut self, global: &BrightnessScale) {
        self.scale.set_value(global.value() * self.scale_factor);
    }

    /// `updateScaleFactor` (`:405-407`).
    fn update_scale_factor(&mut self, max: f64) {
        self.scale_factor = self.scale.value() / max;
    }

    /// `setBacklight` + `_setRelativeBrightness` (`:376-379,397-403`): turn a 0..1 fraction into
    /// the raw value, remembering it so the resulting echo is recognized as ours.
    fn set_backlight(&mut self, backlight: &OutputBacklight, brightness: f64) -> i32 {
        let range = f64::from(backlight.range.max - backlight.range.min);
        let raw = f64::from(backlight.range.min) + range * brightness;
        let raw = backlight.range.clamp(raw.round() as i32);
        self.current_backlight_brightness = raw;
        raw
    }
}

/// `BrightnessManager` (`:14-277`), minus the keybindings and the OSD (Q4d).
///
/// The manager owns no hardware: every entry point takes the current [`BacklightSnapshot`] and
/// returns the writes to perform.
#[derive(Debug, Clone, PartialEq)]
pub struct BrightnessManager {
    /// `None` when no monitor has a backlight — which is also what hides the slider
    /// (`brightness.js:59-60`). Once created it **survives** later monitors-changed with its value
    /// intact (`:163-179`).
    global: Option<BrightnessScale>,
    /// One per backlit output, in snapshot (output) order.
    monitors: Vec<MonitorScale>,
    /// GNOME's `_globalScaleChanged`.
    global_changed: bool,
    dimming_enabled: bool,
    dimming_target: f64,
    /// `_abTarget`: the auto-brightness target, or negative when auto-brightness is off. Set
    /// through the `org.gnome.Shell.Brightness` session object, which is Q4d — so for now it stays
    /// at -1 and the bias below is dormant.
    ab_target: f64,
}

impl Default for BrightnessManager {
    fn default() -> Self {
        Self {
            global: None,
            monitors: Vec::new(),
            global_changed: false,
            dimming_enabled: false,
            dimming_target: DEFAULT_DIMMING_TARGET,
            ab_target: -1.,
        }
    }
}

impl BrightnessManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The global scale, or `None` when nothing is backlit. Its presence is what decides whether
    /// the quick-settings slider exists at all.
    pub fn global_scale(&self) -> Option<&BrightnessScale> {
        self.global.as_ref()
    }

    /// The per-monitor scales, in output order — the detail card's rows.
    pub fn scales(&self) -> &[MonitorScale] {
        &self.monitors
    }

    /// The snapshot the UI renders from.
    pub fn view(&self) -> BrightnessView {
        BrightnessView {
            global: self.global.as_ref().map(BrightnessScale::value),
            monitors: self
                .monitors
                .iter()
                .map(|scale| MonitorView {
                    connector: scale.connector.clone(),
                    name: scale.name().to_owned(),
                    value: scale.value(),
                })
                .collect(),
        }
    }

    pub fn dimming(&self) -> bool {
        self.dimming_enabled
    }

    /// `idle-brightness / 100`, from the power plugin's schema (`:28-29`).
    pub fn set_dimming_target(&mut self, idle_brightness_percent: i32) {
        self.dimming_target = f64::from(idle_brightness_percent) / 100.;
    }

    /// `set dimming` (`:84-87`).
    pub fn set_dimming(&mut self, enable: bool, snapshot: &BacklightSnapshot) -> BrightnessUpdate {
        self.dimming_enabled = enable;
        self.sync(snapshot, true)
    }

    /// `set autoBrightnessTarget` (`:93-96`).
    pub fn set_auto_brightness_target(
        &mut self,
        target: f64,
        snapshot: &BacklightSnapshot,
    ) -> BrightnessUpdate {
        self.ab_target = target;
        self.sync(snapshot, true)
    }

    /// `_monitorsChanged` (`:134-181`): rebuild the per-monitor scales from the outputs that have
    /// a backlight, creating the global scale once and keeping its value forever after.
    pub fn monitors_changed(&mut self, snapshot: &BacklightSnapshot) -> BrightnessUpdate {
        self.monitors = snapshot
            .outputs
            .iter()
            .map(|backlight| {
                let mut scale = MonitorScale::new(backlight, 1.);
                // GNOME marks every fresh scale as changed, so the first `_sync` normalizes the
                // factors from the hardware's values rather than from the 1.0 they start at.
                scale.changed = true;
                scale
            })
            .collect();

        if self.monitors.is_empty() {
            self.global = None;
        } else if self.global.is_none() {
            // Handle scales with just a few steps.
            let max_steps = self
                .monitors
                .iter()
                .map(MonitorScale::n_steps)
                .max()
                .unwrap_or(SCALE_VALUE_N_STEPS);
            let n_steps = max_steps.min(SCALE_VALUE_N_STEPS);
            self.global = Some(BrightnessScale::new("Brightness", 1., n_steps));
        }

        // `_sync({showOSD: false})` (`:181`): a hotplug re-derives every scale, and GNOME does not
        // put an OSD on screen for that.
        self.sync(snapshot, false)
    }

    /// The user moved the global slider (GNOME's `notify::value` on the global scale, `:172-179`).
    pub fn set_global_value(
        &mut self,
        value: f64,
        snapshot: &BacklightSnapshot,
    ) -> BrightnessUpdate {
        let Some(global) = self.global.as_mut() else {
            return BrightnessUpdate::default();
        };
        global.set_value(value);
        self.global_changed = true;
        self.sync(snapshot, true)
    }

    /// The user moved one monitor's slider in the detail card (`:150-158`).
    pub fn set_monitor_value(
        &mut self,
        connector: &str,
        value: f64,
        snapshot: &BacklightSnapshot,
    ) -> BrightnessUpdate {
        let Some(scale) = self.monitors.iter_mut().find(|s| s.connector == connector) else {
            return BrightnessUpdate::default();
        };
        scale.scale.set_value(value);
        scale.changed = true;
        self.sync(snapshot, true)
    }

    /// The hardware moved (GNOME's `backlights-changed`, `:151`): a firmware hotkey, another tool,
    /// or the echo of one of our own writes.
    pub fn backlights_changed(&mut self, snapshot: &BacklightSnapshot) -> BrightnessUpdate {
        self.sync(snapshot, true)
    }

    /// A brightness-up/down/cycle key on the global scale (`:107-132`). The per-monitor variants
    /// take the connector of the monitor the pointer is on.
    pub fn step_global(&mut self, step: Step, snapshot: &BacklightSnapshot) -> BrightnessUpdate {
        let Some(global) = self.global.as_mut() else {
            return BrightnessUpdate::default();
        };
        step.apply(global);
        self.global_changed = true;
        self.sync(snapshot, true)
    }

    pub fn step_monitor(
        &mut self,
        connector: &str,
        step: Step,
        snapshot: &BacklightSnapshot,
    ) -> BrightnessUpdate {
        let Some(scale) = self.monitors.iter_mut().find(|s| s.connector == connector) else {
            return BrightnessUpdate::default();
        };
        step.apply(&mut scale.scale);
        scale.changed = true;
        self.sync(snapshot, true)
    }

    /// `_sync` (`:186-262`). The phase order is load-bearing:
    ///
    /// 1. adopt any hardware change, and let it cancel dimming;
    /// 2. if a monitor scale moved, re-derive every factor from the new maximum and pull the global
    ///    scale to that maximum — *else* if the global scale moved, fan it back out through the
    ///    factors (never both in one pass);
    /// 3. write every monitor, applying the auto-brightness bias and the dimming clamp.
    ///
    /// `show_osd` is GNOME's `_sync({showOSD})` (`:186`): on it, whichever branch of phase 2 ran
    /// also asks for the brightness OSD — the monitor branch for *only* the scales that moved,
    /// the global branch for all of them (`:227-239`).
    fn sync(&mut self, snapshot: &BacklightSnapshot, show_osd: bool) -> BrightnessUpdate {
        let Some(global) = self.global.as_mut() else {
            return BrightnessUpdate::default();
        };

        // Handle changed backlights.
        for scale in &mut self.monitors {
            let Some(backlight) = snapshot.get(&scale.connector) else {
                continue;
            };
            if scale.sync_with_backlight(backlight) {
                // Disable dimming for all if we have a single system initiated backlight change.
                self.dimming_enabled = false;
            }
        }

        // Find scales which have been changed (and reset the flag). GNOME keeps the *list*, not
        // just the fact, because it is what the OSD is shown for.
        let changed_scales: Vec<usize> = self
            .monitors
            .iter_mut()
            .enumerate()
            .filter_map(|(i, scale)| std::mem::take(&mut scale.changed).then_some(i))
            .collect();

        let mut osd = Vec::new();
        if !changed_scales.is_empty() {
            // Normalize everything to the maximum of all scales.
            let max = self
                .monitors
                .iter()
                .map(MonitorScale::value)
                .fold(f64::NEG_INFINITY, f64::max);

            // If max is 0 we can't deduce any ratios, so don't try.
            if max > 0.01 {
                for scale in &mut self.monitors {
                    scale.update_scale_factor(max);
                }
            }

            // The global scale always follows the maximum, because one monitor scale is at the
            // maximum and we want the global scale to be a factor we apply on the ratio of the
            // monitor scales.
            global.set_value(max);

            if show_osd {
                osd = changed_scales
                    .iter()
                    .map(|&i| OsdRequest {
                        connector: self.monitors[i].connector.clone(),
                        level: self.monitors[i].value(),
                    })
                    .collect();
            }
        } else if self.global_changed {
            // If the global scale changed, update the monitor scales according to their
            // scaleFactor and the global scale.
            self.global_changed = false;

            for scale in &mut self.monitors {
                scale.sync_with_scale(global);
            }

            if show_osd {
                osd = self
                    .monitors
                    .iter()
                    .map(|scale| OsdRequest {
                        connector: scale.connector.clone(),
                        level: scale.value(),
                    })
                    .collect();
            }
        }

        // Update the actual backlight according to the new monitor brightnesses and other factors,
        // such as dimming.
        let mut writes = Vec::new();
        for scale in &mut self.monitors {
            let Some(backlight) = snapshot.get(&scale.connector) else {
                continue;
            };

            // If auto brightness is active (ab_target >= 0) we use the scale as a bias for the
            // auto brightness target to determine the target brightness. Otherwise the target
            // brightness just comes from the scale.
            let target = if self.ab_target >= 0. {
                (self.ab_target + scale.value() - 0.5).clamp(0., 1.)
            } else {
                scale.value()
            };

            // The actual brightness is then determined by clipping to the dimming target, if
            // dimming is enabled.
            let max = if self.dimming_enabled {
                self.dimming_target
            } else {
                1.
            };
            let brightness = f64::min(max, target);

            let raw = scale.set_backlight(backlight, brightness);
            writes.push(BacklightWrite {
                connector: backlight.connector.clone(),
                brightness: raw,
            });
        }

        BrightnessUpdate { writes, osd }
    }
}

/// The three brightness keybinding shapes (`:107-132`). The bindings themselves are Q4d.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Up,
    Down,
    /// Steps up, wrapping to zero at the top.
    Cycle,
}

impl Step {
    fn apply(self, scale: &mut BrightnessScale) {
        match self {
            Step::Up => scale.step_up(),
            Step::Down => scale.step_down(),
            Step::Cycle => scale.cycle_up(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backlight::BacklightRange;

    /// A 0..100 backlight, so a scale fraction reads as a percentage of the usable range.
    fn backlight(connector: &str, name: &str, brightness: i32) -> OutputBacklight {
        OutputBacklight {
            connector: connector.to_owned(),
            display_name: name.to_owned(),
            range: BacklightRange { min: 0, max: 100 },
            brightness,
        }
    }

    fn snapshot(outputs: Vec<OutputBacklight>) -> BacklightSnapshot {
        BacklightSnapshot { outputs }
    }

    fn one_panel(brightness: i32) -> BacklightSnapshot {
        snapshot(vec![backlight("eDP-1", "Built-in display", brightness)])
    }

    /// Apply the manager's writes back onto a snapshot, the way the hardware echo would.
    fn apply(snapshot: &mut BacklightSnapshot, update: BrightnessUpdate) {
        for BacklightWrite {
            connector,
            brightness,
        } in update.writes
        {
            let output = snapshot
                .outputs
                .iter_mut()
                .find(|o| o.connector == connector)
                .unwrap();
            output.brightness = brightness;
        }
    }

    fn value_of(manager: &BrightnessManager, connector: &str) -> f64 {
        manager
            .scales()
            .iter()
            .find(|s| s.connector() == connector)
            .unwrap()
            .value()
    }

    #[test]
    fn no_backlight_means_no_global_scale() {
        let mut manager = BrightnessManager::new();
        let empty = BacklightSnapshot::default();
        assert!(manager.monitors_changed(&empty).is_empty());
        assert!(manager.global_scale().is_none());
        assert!(manager.scales().is_empty());

        // Every entry point is a no-op without a global scale.
        assert!(manager.set_global_value(0.5, &empty).is_empty());
        assert!(manager.backlights_changed(&empty).is_empty());
        assert!(manager.step_global(Step::Up, &empty).is_empty());
    }

    #[test]
    fn the_first_sync_adopts_the_hardware_value() {
        let mut manager = BrightnessManager::new();
        let snap = one_panel(40);

        let writes = manager.monitors_changed(&snap);
        // The scale starts at 1.0 but the first sync reads the hardware, so it lands at 40%...
        assert_eq!(value_of(&manager, "eDP-1"), 0.4);
        // ... and the global scale follows the maximum, not the 1.0 it was born with.
        assert_eq!(manager.global_scale().unwrap().value(), 0.4);
        // The write is a no-op round trip of the value we just read.
        assert_eq!(writes.writes[0].brightness, 40);
    }

    #[test]
    fn a_hardware_change_cancels_dimming() {
        let mut manager = BrightnessManager::new();
        let mut snap = one_panel(100);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        let writes = manager.set_dimming(true, &snap);
        assert!(manager.dimming());
        // Clamped to the 30% idle target, without moving the scale itself.
        assert_eq!(writes.writes[0].brightness, 30);
        assert_eq!(value_of(&manager, "eDP-1"), 1.0);
        apply(&mut snap, writes);

        // Now someone hits the firmware brightness key.
        snap.outputs[0].brightness = 70;
        let writes = manager.backlights_changed(&snap);
        assert!(!manager.dimming());
        assert_eq!(value_of(&manager, "eDP-1"), 0.7);
        assert_eq!(writes.writes[0].brightness, 70);
    }

    #[test]
    fn our_own_write_echo_is_not_a_hardware_change() {
        let mut manager = BrightnessManager::new();
        let mut snap = one_panel(100);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        let writes = manager.set_dimming(true, &snap);
        apply(&mut snap, writes);
        assert!(manager.dimming());

        // The echo of the dimming write arrives. If it were treated as a system change it would
        // cancel the dimming it just applied, and the screen would bounce back up.
        let writes = manager.backlights_changed(&snap);
        assert!(manager.dimming());
        assert_eq!(writes.writes[0].brightness, 30);
        assert_eq!(value_of(&manager, "eDP-1"), 1.0);
    }

    #[test]
    fn the_global_slider_preserves_the_ratio_between_monitors() {
        let mut manager = BrightnessManager::new();
        let mut snap = snapshot(vec![
            backlight("eDP-1", "Built-in display", 100),
            backlight("DP-2", "Dell 24″", 50),
        ]);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        // The factors come out of the first sync: the dimmer monitor is at half the brighter one.
        assert_eq!(manager.global_scale().unwrap().value(), 1.0);
        assert_eq!(value_of(&manager, "DP-2"), 0.5);

        // Halving the global slider halves both, keeping the 2:1 ratio.
        let writes = manager.set_global_value(0.5, &snap);
        assert_eq!(value_of(&manager, "eDP-1"), 0.5);
        assert_eq!(value_of(&manager, "DP-2"), 0.25);
        assert_eq!(writes.writes[0].brightness, 50);
        assert_eq!(writes.writes[1].brightness, 25);
    }

    #[test]
    fn moving_one_monitor_rederives_the_factors_and_pulls_the_global_scale() {
        let mut manager = BrightnessManager::new();
        let mut snap = snapshot(vec![
            backlight("eDP-1", "Built-in display", 100),
            backlight("DP-2", "Dell 24″", 50),
        ]);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        // Push the external monitor past the panel: it becomes the new maximum, so the global
        // scale follows it and the panel's factor is re-derived against it.
        let writes = manager.set_monitor_value("DP-2", 1.0, &snap);
        assert_eq!(manager.global_scale().unwrap().value(), 1.0);
        assert_eq!(value_of(&manager, "eDP-1"), 1.0);
        assert_eq!(writes.writes[1].brightness, 100);
        apply(&mut snap, writes);

        // The factors are now 1:1, so the global slider moves them together.
        let writes = manager.set_global_value(0.4, &snap);
        assert_eq!(value_of(&manager, "eDP-1"), 0.4);
        assert_eq!(value_of(&manager, "DP-2"), 0.4);
        assert_eq!(writes.writes[0].brightness, 40);
        assert_eq!(writes.writes[1].brightness, 40);
    }

    #[test]
    fn a_monitor_change_wins_over_a_global_change_in_the_same_pass() {
        // GNOME's `_sync` is an if/else: when a monitor scale moved, the global-changed branch is
        // skipped entirely (and its flag is left standing for the next pass).
        let mut manager = BrightnessManager::new();
        let mut snap = snapshot(vec![
            backlight("eDP-1", "Built-in display", 100),
            backlight("DP-2", "Dell 24″", 100),
        ]);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        manager.global_changed = true;
        manager
            .monitors
            .iter_mut()
            .for_each(|s| s.scale.set_value(0.5));
        manager.monitors[0].changed = true;

        let writes = manager.sync(&snap, true);
        // Normalized against the max (0.5), not fanned out from the untouched global value.
        assert_eq!(manager.global_scale().unwrap().value(), 0.5);
        assert_eq!(writes.writes[0].brightness, 50);
        assert_eq!(writes.writes[1].brightness, 50);
    }

    #[test]
    fn an_all_dark_pass_keeps_the_old_factors() {
        let mut manager = BrightnessManager::new();
        let mut snap = snapshot(vec![
            backlight("eDP-1", "Built-in display", 100),
            backlight("DP-2", "Dell 24″", 50),
        ]);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        // Drag the panel to zero. The maximum is now the external monitor, so the factors are
        // re-derived against it: the panel is 0 of it, the monitor is all of it.
        let writes = manager.set_monitor_value("eDP-1", 0., &snap);
        apply(&mut snap, writes);
        assert_eq!(manager.monitors[0].scale_factor, 0.);
        assert_eq!(manager.monitors[1].scale_factor, 1.);

        // Now drag the last lit monitor to zero too. There is no ratio to deduce from an all-zero
        // set, so the guard leaves the factors standing rather than dividing by zero.
        let writes = manager.set_monitor_value("DP-2", 0., &snap);
        apply(&mut snap, writes);
        assert_eq!(manager.monitors[0].scale_factor, 0.);
        assert_eq!(manager.monitors[1].scale_factor, 1.);
        assert_eq!(manager.global_scale().unwrap().value(), 0.);

        // Coming back up through the global slider fans out through those surviving factors.
        let writes = manager.set_global_value(1., &snap);
        assert_eq!(writes.writes[0].brightness, 0);
        assert_eq!(writes.writes[1].brightness, 100);
    }

    #[test]
    fn the_auto_brightness_target_biases_around_the_middle() {
        let mut manager = BrightnessManager::new();
        let mut snap = one_panel(50);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        // With the scale at its midpoint the target is exactly the auto-brightness value.
        let writes = manager.set_auto_brightness_target(0.8, &snap);
        assert_eq!(writes.writes[0].brightness, 80);
        apply(&mut snap, writes);

        // Above the midpoint the scale biases the target upwards, and clamps at 1.
        let writes = manager.set_monitor_value("eDP-1", 1.0, &snap);
        assert_eq!(writes.writes[0].brightness, 100);
        apply(&mut snap, writes);

        // A negative target turns auto-brightness back off: the scale is the target again.
        let writes = manager.set_auto_brightness_target(-1., &snap);
        assert_eq!(writes.writes[0].brightness, 100);
    }

    #[test]
    fn the_global_scale_survives_monitors_changing() {
        let mut manager = BrightnessManager::new();
        let mut snap = one_panel(100);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        let writes = manager.set_global_value(0.6, &snap);
        apply(&mut snap, writes);
        assert_eq!(manager.global_scale().unwrap().value(), 0.6);

        // Plugging in a second backlit monitor rebuilds the monitor scales, but the global scale
        // is created once and keeps its value.
        let mut snap = snapshot(vec![
            backlight("eDP-1", "Built-in display", 60),
            backlight("DP-2", "Dell 24″", 30),
        ]);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);
        assert_eq!(manager.scales().len(), 2);
        // The fresh scales are all marked changed, so the global scale re-normalizes to the new
        // maximum -- which is the panel, still at 60%.
        assert_eq!(manager.global_scale().unwrap().value(), 0.6);

        // Unplugging the last backlit monitor drops the global scale entirely, hiding the slider.
        assert!(manager
            .monitors_changed(&BacklightSnapshot::default())
            .is_empty());
        assert!(manager.global_scale().is_none());
    }

    #[test]
    fn a_coarse_backlight_gets_a_coarse_scale() {
        // A backlight with 3 usable steps must not get a 20-step slider.
        let mut manager = BrightnessManager::new();
        let snap = snapshot(vec![OutputBacklight {
            connector: "eDP-1".to_owned(),
            display_name: "Built-in display".to_owned(),
            range: BacklightRange { min: 0, max: 3 },
            brightness: 3,
        }]);
        let _ = manager.monitors_changed(&snap);

        assert_eq!(manager.scales()[0].n_steps(), 3);
        assert_eq!(manager.global_scale().unwrap().n_steps(), 3);

        // ... and a fine one is capped at 20.
        let mut manager = BrightnessManager::new();
        let _ = manager.monitors_changed(&one_panel(100));
        assert_eq!(manager.scales()[0].n_steps(), 20);
        assert_eq!(manager.global_scale().unwrap().n_steps(), 20);
    }

    #[test]
    fn stepping_walks_the_scale_and_cycling_wraps() {
        let mut scale = BrightnessScale::new("Brightness", 1., 20);

        scale.step_up();
        assert_eq!(scale.value(), 1.0); // already at the top

        scale.step_down();
        assert!((scale.value() - 0.95).abs() < 1e-9);

        // Cycling from the top wraps to zero; from anywhere else it is a step up.
        scale.set_value(1.);
        scale.cycle_up();
        assert_eq!(scale.value(), 0.);
        scale.cycle_up();
        assert!((scale.value() - 0.05).abs() < 1e-9);

        // The epsilon means "near enough to the top" wraps too.
        scale.set_value(1. - SCALE_VALUE_CHANGE_EPSILON / 2.);
        scale.cycle_up();
        assert_eq!(scale.value(), 0.);

        // Stepping down bottoms out at zero rather than going negative.
        scale.set_value(0.);
        scale.step_down();
        assert_eq!(scale.value(), 0.);
    }

    #[test]
    fn keys_drive_the_same_algebra_as_the_sliders() {
        let mut manager = BrightnessManager::new();
        let mut snap = snapshot(vec![
            backlight("eDP-1", "Built-in display", 100),
            backlight("DP-2", "Dell 24″", 50),
        ]);
        let writes = manager.monitors_changed(&snap);
        apply(&mut snap, writes);

        // A global step moves both monitors, through the factors.
        let writes = manager.step_global(Step::Down, &snap);
        assert_eq!(writes.writes[0].brightness, 95);
        assert_eq!(writes.writes[1].brightness, 48); // 0.475 of the range, rounded
        apply(&mut snap, writes);

        // A per-monitor step re-normalizes, so the global scale follows the new maximum.
        let writes = manager.step_monitor("DP-2", Step::Up, &snap);
        assert_eq!(writes.writes[1].brightness, 53);
        assert_eq!(manager.global_scale().unwrap().value(), 0.95);

        // An unknown connector is a no-op.
        assert!(manager.step_monitor("HDMI-A-1", Step::Up, &snap).is_empty());
    }

    #[test]
    fn writes_stay_inside_the_usable_range() {
        // A panel whose minimum is above zero: the scale's 0 must land on the minimum, not on a
        // value that would turn the panel off.
        let mut manager = BrightnessManager::new();
        let snap = snapshot(vec![OutputBacklight {
            connector: "eDP-1".to_owned(),
            display_name: "Built-in display".to_owned(),
            range: BacklightRange { min: 10, max: 110 },
            brightness: 60,
        }]);
        let writes = manager.monitors_changed(&snap);
        assert_eq!(writes.writes[0].brightness, 60);
        assert_eq!(value_of(&manager, "eDP-1"), 0.5);

        let writes = manager.set_global_value(0., &snap);
        assert_eq!(writes.writes[0].brightness, 10);

        let writes = manager.set_global_value(1., &snap);
        assert_eq!(writes.writes[0].brightness, 110);
    }
}
