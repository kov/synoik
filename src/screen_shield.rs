//! The screen shield — `ScreenShield` (`js/ui/screenShield.js`).
//!
//! GNOME's session lock is the shell's own, not an external locker's: gnome-shell *is* the lock
//! screen. So this is a model the compositor owns, driven from D-Bus
//! (`org.gnome.ScreenSaver`, see `dbus::gnome_screen_saver`), from the idle machinery, and from
//! logind.
//!
//! **Two booleans, not one.** `active` is "the shield is down" and `locked` is "getting back in
//! needs authentication", and GNOME really does run `active && !locked`: that is what a blanked
//! screen with `org.gnome.desktop.screensaver lock-enabled = false` is, and what a user whose
//! `password_mode` is NONE always gets (`lock`, `:637-661`). Collapsing them would either demand
//! a password from someone who has none or blank without ever locking.
//!
//! # Safety rail — see [`ScreenShield::lock`]
//!
//! Entering `locked` with no way to authenticate is a lockout: the shield covers the screen and
//! nothing short of a VT switch gets you back. Until the unlock dialog lands, [`lock`] refuses to
//! set `locked` and says so. That is a real divergence and it is deliberate; the alternative is a
//! lock that traps whoever tries it.
//!
//! [`lock`]: ScreenShield::lock

use std::time::Duration;

/// What changed, for the caller to publish. Returned rather than emitted so the model stays
/// testable without a bus — the same shape [`crate::ui::switcher`] uses for its outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShieldEffects {
    /// `ActiveChanged` — emitted only on a real edge (`_setActive`, `:156-164`).
    pub active_changed: Option<bool>,
    /// `locked-changed`, whose one consumer is logind's `SetLockedHint` (`_setLocked`,
    /// `:166-175`).
    pub locked_changed: Option<bool>,
    /// `WakeUpScreen` (`_wakeUpScreen`, `:495-501`).
    pub wake_up_screen: bool,
    /// Lowering the shield clears the clipboard, so its contents cannot be leaked by pasting into
    /// the unlock entry and unmasking it (`lock`, `:648-651`). Both selections, as GNOME does.
    pub clear_clipboard: bool,
}

/// The lockdown and screensaver settings [`ScreenShield`] consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShieldSettings {
    /// `org.gnome.desktop.lockdown disable-lock-screen` — when set, [`ScreenShield::lock`] does
    /// nothing at all (`:638-641`).
    pub disable_lock_screen: bool,
    /// `org.gnome.desktop.screensaver lock-enabled` — whether going idle should end up locked.
    /// Consulted by the idle path, not by an explicit `Lock`.
    pub lock_enabled: bool,
}

impl Default for ShieldSettings {
    fn default() -> Self {
        // GNOME's schema defaults: locking is on, lockdown is off.
        Self {
            disable_lock_screen: false,
            lock_enabled: true,
        }
    }
}

/// Why a [`ScreenShield::lock`] did not lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockRefused {
    /// `disable-lock-screen` is set — GNOME logs "Screen lock is locked down, not locking".
    LockedDown,
}

#[derive(Debug, Default)]
pub struct ScreenShield {
    active: bool,
    locked: bool,
    /// `_activationTime`, monotonic. `None` is GNOME's 0.
    activation_time: Option<Duration>,
    settings: ShieldSettings,
    /// See the safety rail in the module docs. Cleared when the unlock dialog exists.
    can_authenticate: bool,
}

impl ScreenShield {
    pub fn new(settings: ShieldSettings) -> Self {
        Self {
            settings,
            ..Default::default()
        }
    }

    pub fn set_settings(&mut self, settings: ShieldSettings) {
        self.settings = settings;
    }

    pub fn settings(&self) -> ShieldSettings {
        self.settings
    }

    /// `get active` (`:507-509`).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// `_activationTime`, for the snapshot the bus reads.
    pub fn activation_time(&self) -> Option<Duration> {
        self.activation_time
    }

    /// `get locked` (`:503-505`).
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// `GetActiveTime` (`shellDBus.js:558-565`): whole seconds since the shield went down, or 0.
    pub fn active_time_secs(&self, now: Duration) -> u32 {
        let Some(started) = self.activation_time else {
            return 0;
        };
        now.saturating_sub(started).as_secs() as u32
    }

    /// `activate` (`:586-616`) — put the shield down *without* requiring authentication.
    ///
    /// This is the screensaver half: `SetActive(true)`, and the idle fade. It never sets `locked`;
    /// only [`lock`](Self::lock) does.
    pub fn activate(&mut self, now: Duration) -> ShieldEffects {
        if self.activation_time.is_none() {
            self.activation_time = Some(now);
        }
        self.set_active(true)
    }

    /// `deactivate` (`:515-538`, `_continueDeactivate`) — raise the shield.
    ///
    /// Clears `locked` too: GNOME only reaches here once the dialog has authenticated (or was
    /// never needed), so the caller owes that check, not this.
    pub fn deactivate(&mut self) -> ShieldEffects {
        self.activation_time = None;
        let mut effects = self.set_active(false);
        effects.wake_up_screen = true;
        let unlocked = self.set_locked(false);
        effects.locked_changed = unlocked.locked_changed;
        effects
    }

    /// `lock` (`:637-661`) — put the shield down and require authentication to raise it.
    ///
    /// `password_mode_none` is AccountsService's `password_mode == NONE`: a user with no password
    /// is activated but never locked, because there would be nothing to authenticate with.
    ///
    /// **Does not currently set `locked`.** See the module docs: until the unlock dialog exists,
    /// locking would trap the session. The clipboard is still cleared and the shield still goes
    /// down, so the observable half that is safe to ship is shipped.
    pub fn lock(
        &mut self,
        now: Duration,
        password_mode_none: bool,
    ) -> Result<ShieldEffects, LockRefused> {
        if self.settings.disable_lock_screen {
            return Err(LockRefused::LockedDown);
        }

        let mut effects = self.activate(now);
        effects.clear_clipboard = true;

        let wants_lock = !password_mode_none && self.can_authenticate;
        let locked = self.set_locked(wants_lock);
        effects.locked_changed = locked.locked_changed;
        Ok(effects)
    }

    /// `_wakeUpScreen` (`:495-501`) — user activity while the shield is down.
    ///
    /// Only meaningful while active; GNOME returns early otherwise ("already woken up, or not yet
    /// asleep").
    pub fn wake_up_screen(&mut self) -> ShieldEffects {
        if !self.active {
            return ShieldEffects::default();
        }
        ShieldEffects {
            wake_up_screen: true,
            ..Default::default()
        }
    }

    fn set_active(&mut self, active: bool) -> ShieldEffects {
        let changed = self.active != active;
        self.active = active;
        ShieldEffects {
            active_changed: changed.then_some(active),
            ..Default::default()
        }
    }

    fn set_locked(&mut self, locked: bool) -> ShieldEffects {
        let changed = self.locked != locked;
        self.locked = locked;
        ShieldEffects {
            locked_changed: changed.then_some(locked),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Duration = Duration::from_secs(1_000);

    /// `active` and `locked` are different states, and the screensaver half never demands a
    /// password.
    ///
    /// `SetActive(true)` goes through `activate`, which does not touch `_isLocked` at all
    /// (`:586-616`) — so a blanked screen with `lock-enabled = false` raises on any input. Fusing
    /// the two would put a password prompt in front of a screensaver.
    #[test]
    fn activating_the_shield_does_not_lock_it() {
        let mut shield = ScreenShield::new(ShieldSettings::default());

        let effects = shield.activate(T0);
        assert_eq!(effects.active_changed, Some(true));
        assert_eq!(effects.locked_changed, None);
        assert!(shield.is_active());
        assert!(!shield.is_locked(), "the screensaver is not a lock");

        // And it is idempotent: `ActiveChanged` is edge-triggered (`_setActive`, `:156-164`).
        assert_eq!(
            shield.activate(T0 + Duration::from_secs(5)).active_changed,
            None
        );
        assert_eq!(
            shield.active_time_secs(T0 + Duration::from_secs(5)),
            5,
            "and the activation time is the *first* one, not the latest"
        );
    }

    /// Raising the shield reports the wake-up and resets the activation clock.
    #[test]
    fn deactivating_wakes_the_screen_and_resets_the_clock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.activate(T0);

        let effects = shield.deactivate();
        assert_eq!(effects.active_changed, Some(false));
        assert!(effects.wake_up_screen);
        assert!(!shield.is_active());
        assert_eq!(
            shield.active_time_secs(T0 + Duration::from_secs(60)),
            0,
            "GetActiveTime is 0 when the shield is up"
        );
    }

    /// `disable-lock-screen` refuses the lock outright — the shield does not even go down.
    ///
    /// GNOME returns *before* `activate` (`:638-641`), so a locked-down session does not get a
    /// blanked screen out of a `Lock` call either.
    #[test]
    fn lockdown_refuses_to_lock_at_all() {
        let mut shield = ScreenShield::new(ShieldSettings {
            disable_lock_screen: true,
            ..ShieldSettings::default()
        });

        assert_eq!(shield.lock(T0, false), Err(LockRefused::LockedDown));
        assert!(!shield.is_active(), "and the shield stays up");
        assert!(!shield.is_locked());
    }

    /// A lock clears the clipboard, both selections.
    ///
    /// `lock` empties CLIPBOARD and PRIMARY before showing the dialog (`:645-651`) precisely
    /// because the unlock entry can be unmasked — so a password left in the clipboard would be
    /// readable by anyone who walked up. Losing this is silent and only matters to an attacker.
    #[test]
    fn locking_clears_the_clipboard() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let effects = shield.lock(T0, false).expect("not locked down");
        assert!(effects.clear_clipboard);
        assert!(shield.is_active());
    }

    /// A user with no password is activated, never locked (`:656-659`).
    #[test]
    fn a_passwordless_user_is_never_locked() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.can_authenticate = true;

        let effects = shield.lock(T0, true).expect("not locked down");
        assert!(shield.is_active());
        assert!(!shield.is_locked());
        assert_eq!(effects.locked_changed, None);

        // ...where a user who has one is.
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.can_authenticate = true;
        let effects = shield.lock(T0, false).expect("not locked down");
        assert!(shield.is_locked());
        assert_eq!(effects.locked_changed, Some(true));
    }

    /// The safety rail: with no way to authenticate, `lock` activates but refuses to lock.
    ///
    /// This is a deliberate divergence and the reason it is a named field rather than a comment —
    /// when the unlock dialog lands, this test is what says where to flip it.
    #[test]
    fn locking_without_an_unlock_path_activates_but_does_not_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        assert!(!shield.can_authenticate, "no unlock dialog yet");

        let effects = shield.lock(T0, false).expect("not locked down");
        assert!(shield.is_active(), "the shield still goes down");
        assert!(
            !shield.is_locked(),
            "but it must not lock: there is nothing to unlock it with"
        );
        assert_eq!(effects.locked_changed, None);
    }
}
