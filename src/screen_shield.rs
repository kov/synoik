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
//! # The lock gate
//!
//! Entering `locked` with no way to authenticate is a lockout: the shield covers the screen and
//! nothing short of a VT switch gets you back. gdm being down, the reauthentication channel being
//! denied and PAM being misconfigured all look identical from inside the compositor, and all of
//! them are silent.
//!
//! So `locked` is not gated on a flag someone remembered to set. [`lock`] puts the shield **down**
//! and asks for a verifier; only [`authenticator_ready`] — driven by a real open channel from
//! [`crate::dbus::gdm`] — turns that into `locked`. A shield with no verifier stays a screensaver:
//! it covers the screen and raises on any input, which is the honest thing to be.
//!
//! [`lock`]: ScreenShield::lock
//! [`authenticator_ready`]: ScreenShield::authenticator_ready

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
    /// Open a gdm reauthentication channel for the session user, tagged with this epoch. The
    /// shield is now *active* and waiting to hear whether it may become *locked* — see
    /// [`ScreenShield::authenticator_ready`].
    pub request_authenticator: Option<u64>,
    /// Tear any conversation down: the shield was raised, so gdm's PAM worker and its channel must
    /// not outlive it.
    pub cancel_authenticator: bool,
    /// Arm a one-shot timer that calls [`ScreenShield::lock`] after this long
    /// (`_onStatusChanged`, `:257-271`). Replaces any timer already pending.
    pub arm_lock_timer: Option<Duration>,
    /// Drop a pending lock timer — the shield was raised before it could fire.
    pub cancel_lock_timer: bool,
    /// `_maybeCancelDialog` (`:186-191`) — put the prompt page back to the clock, so an
    /// unattended screen is not left holding a half-typed password.
    pub cancel_dialog: bool,
    /// Start the long fade to black (`_activateFade`, `:275-283`). The shield goes down when it
    /// finishes, not when it starts — see [`ScreenShield::fade_complete`].
    pub start_fade: bool,
    /// Drop the fade at once (`lightOff` with no duration, `lightbox.js:223-227`).
    pub stop_fade: bool,
    /// Put the curtain down *without* its slide (`activate(false)`).
    ///
    /// The idle path is the reason this exists: by the time the shield goes down the screen is
    /// already black from the fade, so sliding a curtain onto it would be a second animation over
    /// a picture nobody can see.
    pub curtain_instant: bool,
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
    /// `org.gnome.desktop.lockdown disable-show-password` — removes the unlock entry's peek
    /// toggle (`st-password-entry.c:61-68`).
    pub disable_show_password: bool,
    /// `org.gnome.desktop.screensaver lock-delay` — the grace period between the session going
    /// idle and the shield actually locking, so a user who comes straight back is not challenged.
    pub lock_delay: Duration,
    /// `org.gnome.desktop.screensaver user-switch-enabled` — whether the unlock dialog offers to
    /// log in as somebody else (`unlockDialog.js:922-925`).
    pub user_switch_enabled: bool,
    /// `org.gnome.desktop.lockdown disable-user-switching` — the administrator's veto over the
    /// same button. Both are consulted, and either one being against it wins.
    pub disable_user_switching: bool,
}

/// `STANDARD_FADE_TIME` (`:39`) — the fade GNOME runs when the session goes idle.
///
/// It is a floor on the lock delay, not just an animation: `_onStatusChanged` locks after
/// `max(STANDARD_FADE_TIME, lock-delay)`, so the default `lock-delay = 0` still leaves ten seconds
/// rather than locking the instant the session is declared idle.
pub const STANDARD_FADE_TIME: Duration = Duration::from_secs(10);

impl Default for ShieldSettings {
    fn default() -> Self {
        // GNOME's schema defaults: locking is on, lockdown is off.
        Self {
            disable_lock_screen: false,
            lock_enabled: true,
            disable_show_password: false,
            lock_delay: Duration::ZERO,
            user_switch_enabled: true,
            disable_user_switching: false,
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
    /// The epoch a [`lock`](Self::lock) is waiting on a verifier for, if any.
    ///
    /// An epoch rather than a bool because opening a channel is a round trip that can outlive the
    /// lock that asked for it: lock, dismiss, lock again, and the *first* answer would otherwise
    /// satisfy the second lock's gate — covered-but-unlocked if it failed, or locked with no
    /// conversation behind it if it succeeded.
    awaiting_authenticator: Option<u64>,
    /// Monotonically increasing; one per lock that asks for a verifier.
    epoch: u64,
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

    /// Whether a keypress or click should raise the shield.
    ///
    /// **Not simply `!locked`.** Between [`lock`](Self::lock) and
    /// [`authenticator_ready`](Self::authenticator_ready) the shield is down and `locked` is still
    /// false, because the gate is a live gdm channel and opening one is a round trip. Treating that
    /// window as a screensaver means a lock can be dismissed by whoever gets a key in first — walk
    /// up to a machine that just suspended, wiggle the mouse, and you are past it.
    ///
    /// GNOME has no such window: its `lock()` sets `_isLocked` synchronously (`:660`), so this
    /// predicate is what keeps our asynchronous gate as strict as its synchronous one. If the
    /// channel comes back refused the wait ends and the shield goes back to being dismissible,
    /// which is the screensaver it was always entitled to be.
    pub fn is_dismissible(&self) -> bool {
        self.active && !self.is_challenging()
    }

    /// Whether getting back in would take a password — either because the shield is locked, or
    /// because it has asked for a verifier and not heard back.
    ///
    /// Separate from [`is_dismissible`](Self::is_dismissible) because that one also requires the
    /// shield to be *down*, and there is a state where nothing is down yet but a lock is already
    /// pending: the ten-second idle fade. Using the stricter predicate there leaves the armed lock
    /// running when the user comes back, and the desktop locks under them a few seconds later.
    fn is_challenging(&self) -> bool {
        self.locked || self.awaiting_authenticator.is_some()
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
        self.awaiting_authenticator = None;
        let mut effects = self.set_active(false);
        effects.wake_up_screen = true;
        effects.cancel_authenticator = true;
        // `_completeDeactivate` drops the pending lock (`:575-578`). Leaving it armed would lock a
        // desktop the user is sitting at, seconds after they dismissed the screensaver.
        effects.cancel_lock_timer = true;
        effects.stop_fade = true;
        let unlocked = self.set_locked(false);
        effects.locked_changed = unlocked.locked_changed;
        effects
    }

    /// `lock` (`:637-661`) — put the shield down and require authentication to raise it.
    ///
    /// `password_mode_none` is AccountsService's `password_mode == NONE`: a user with no password
    /// is activated but never locked, because there would be nothing to authenticate with.
    ///
    /// **Does not set `locked` here.** See the module docs: the shield goes down immediately (a
    /// lock must not wait on a D-Bus round trip to cover the screen) and asks for a verifier;
    /// `locked` follows in [`authenticator_ready`](Self::authenticator_ready).
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

        // A user with no password is activated but never locked (`:656-659`) — there would be
        // nothing to authenticate with, so there is nothing to ask gdm for either.
        if !password_mode_none {
            self.epoch += 1;
            self.awaiting_authenticator = Some(self.epoch);
            effects.request_authenticator = Some(self.epoch);
        }
        Ok(effects)
    }

    /// The verifier answered: `true` if a reauthentication channel is open and the conversation
    /// has begun, `false` if it could not be had.
    ///
    /// This is the only path into `locked`. On `false` the shield stays merely active, so the
    /// screen is still covered and any input raises it.
    pub fn authenticator_ready(&mut self, epoch: u64, ready: bool) -> ShieldEffects {
        // Ignore an answer for a lock that no longer applies: a different epoch, or a shield that
        // has since been raised. Either way, locking now would be a lock nobody asked for.
        if self.awaiting_authenticator != Some(epoch) || !self.active {
            return ShieldEffects::default();
        }
        self.awaiting_authenticator = None;
        if !ready {
            return ShieldEffects::default();
        }
        self.set_locked(true)
    }

    /// The channel behind a locked shield died (gdm went away). There is nothing left to
    /// authenticate against, so the lock is a trap — drop back to a plain screensaver, which the
    /// user can at least dismiss.
    ///
    /// A divergence: GNOME leaves the dialog stuck. Ours is the safer failure, and it is not a
    /// weakening — anyone who can kill gdm as root can already read the session.
    pub fn authenticator_lost(&mut self) -> ShieldEffects {
        self.awaiting_authenticator = None;
        if !self.locked {
            return ShieldEffects::default();
        }
        warn!("the unlock channel died; dropping the lock to a screensaver so it is not a trap");
        self.set_locked(false)
    }

    /// `_onStatusChanged` with `PresenceStatus.IDLE` (`:242-272`) — gnome-session says the seat
    /// has gone idle.
    ///
    /// The idle *threshold* is not ours: gnome-session owns `org.gnome.desktop.session idle-delay`
    /// and tells us when it has elapsed. All we decide is what happens next — cover the screen, and
    /// arm a timer to turn that cover into a lock.
    ///
    /// Nothing goes down here: idle raises a black lightbox over the desktop and fades it in over
    /// ten seconds, and only its completion activates the shield
    /// ([`fade_complete`](Self::fade_complete)).
    pub fn on_session_idle(&mut self, now: Duration) -> ShieldEffects {
        let mut effects = ShieldEffects {
            cancel_dialog: true,
            ..Default::default()
        };

        // GNOME arms whenever `lockEnabled && !isLocked` (`:258-271`) — note it does *not* check
        // whether the shield is already down. An already-covered screensaver going idle still gets
        // locked, and guarding on `active` here would leave `lock-enabled = true` sessions covered
        // but permanently unlocked. Its only guard is the fade already running (`:248-251`), which
        // for us is a lock already in flight.
        if self.settings.lock_enabled && !self.locked && self.awaiting_authenticator.is_none() {
            effects.arm_lock_timer = Some(self.settings.lock_delay.max(STANDARD_FADE_TIME));
        }

        // The shield does **not** go down here. Going idle starts a ten-second fade to black over
        // the desktop, and only when that finishes does `_onLongLightbox` call `activate(false)`
        // (`:271-275`, `:311-314`). Covering the screen the moment the session is declared idle is
        // the obvious shortcut and it is wrong twice: the user loses the gradual warning that the
        // machine is about to blank, and the ten seconds they have to move the mouse and cancel it
        // turn into no seconds at all.
        //
        // `_activationTime` is stamped here, when the session goes idle — not when the shield
        // finally covers the screen ten seconds later (`:257-258`). `GetActiveTime` is what
        // gnome-session and gsd read to decide how long the seat has been unattended, and starting
        // it at the fade's *end* would under-report every idle screensaver by that fade.
        if self.activation_time.is_none() {
            self.activation_time = Some(now);
        }

        if !self.active {
            effects.start_fade = true;
        }
        effects
    }

    /// The long fade reached black: put the shield down (`_onLongLightbox`, `:311-314`).
    ///
    /// Without the slide, because the screen it would slide onto is already black.
    pub fn fade_complete(&mut self, now: Duration) -> ShieldEffects {
        let mut effects = self.activate(now);
        effects.curtain_instant = true;
        effects.stop_fade = true;
        effects
    }

    /// `_onUserBecameActive` (`:285-305`) — the seat came back before the shield became a lock.
    ///
    /// A *locked* shield stays put; there is nothing to undo but the fade, which we do not have.
    /// An unlocked one is a screensaver, so it goes away and takes the pending lock timer with it —
    /// that cancellation is the whole point of the grace period.
    pub fn on_user_active(&mut self) -> ShieldEffects {
        if self.is_challenging() {
            // Still drop a fade that had started: a locked shield has nothing to fade *to*, and a
            // half-black overlay left over it would darken the unlock prompt.
            return ShieldEffects {
                stop_fade: true,
                ..Default::default()
            };
        }
        let mut effects = self.deactivate();
        effects.stop_fade = true;
        effects
    }

    /// `_prepareForSleep` (`:233-241`) — logind's `PrepareForSleep`.
    ///
    /// `true` is the *delay* phase: the machine is about to suspend and is waiting on our inhibitor
    /// (see [`wants_sleep_inhibitor`](Self::wants_sleep_inhibitor)), so this is the last moment at
    /// which locking still happens before the screen contents are left on a sleeping machine.
    /// `false` is the resume, where all that is owed is waking the screen up.
    ///
    /// Unlike the idle path this locks immediately: there is no grace period on a suspend.
    pub fn prepare_for_sleep(
        &mut self,
        about_to_suspend: bool,
        now: Duration,
        password_mode_none: bool,
    ) -> ShieldEffects {
        if !about_to_suspend {
            return self.wake_up_screen();
        }

        if !self.settings.lock_enabled {
            return ShieldEffects {
                cancel_dialog: true,
                ..Default::default()
            };
        }

        // A refusal (`disable-lock-screen`) is not an error here — it is the administrator's
        // answer, and `lock` has already logged it.
        let mut effects = self.lock(now, password_mode_none).unwrap_or_default();
        effects.cancel_dialog = true;
        effects
    }

    /// `_syncInhibitor` (`:202-231`) — whether logind's `delay` sleep inhibitor should be held.
    ///
    /// The inhibitor is what buys the handshake: holding it makes logind send `PrepareForSleep`
    /// and *wait* for us to release it, which is the only reason [`prepare_for_sleep`] gets to lock
    /// before the machine goes down. So it is held exactly while a future suspend would still owe a
    /// lock — an already-active shield owes nothing, and neither does a session that has been told
    /// not to lock.
    ///
    /// `session_active` is logind's `Session.Active`: a session on an inactive VT must not hold up
    /// everyone else's suspend.
    pub fn wants_sleep_inhibitor(&self, session_active: bool) -> bool {
        // `!self.active` is GNOME's condition (`:205-207`), plus the window its synchronous lock
        // does not have: a shield that is down but still waiting on its verifier has not finished
        // locking, and releasing the fd there lets the machine suspend mid-handshake. logind's
        // `InhibitDelayMaxSec` bounds how long that can hold anyone up.
        let lock_finished = self.active && self.awaiting_authenticator.is_none();
        session_active
            && !lock_finished
            && self.settings.lock_enabled
            && !self.settings.disable_lock_screen
    }

    /// `_wakeUpScreen` (`:495-501`) — user activity while the shield is down.
    ///
    /// Only meaningful while active; GNOME returns early otherwise ("already woken up, or not yet
    /// asleep").
    pub fn wake_up_screen(&mut self) -> ShieldEffects {
        if !self.active {
            return ShieldEffects::default();
        }
        // `_wakeUpScreen` runs `_onUserBecameActive` before emitting (`:499`), so waking a shield
        // that is only a screensaver raises it rather than leaving the desktop covered.
        let mut effects = self.on_user_active();
        effects.wake_up_screen = true;
        effects
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

    /// A user with no password is activated, never locked (`:656-659`) — and no verifier is even
    /// asked for.
    #[test]
    fn a_passwordless_user_is_never_locked() {
        let mut shield = ScreenShield::new(ShieldSettings::default());

        let effects = shield.lock(T0, true).expect("not locked down");
        assert!(shield.is_active());
        assert!(!shield.is_locked());
        assert_eq!(effects.locked_changed, None);
        assert!(
            effects.request_authenticator.is_none(),
            "there is nothing to authenticate with, so gdm is not asked"
        );

        // And a late `true` cannot lock them anyway: nothing was pending.
        assert_eq!(shield.authenticator_ready(1, true).locked_changed, None);
        assert!(!shield.is_locked());
    }

    /// **The lock gate.** `lock` puts the shield down and asks for a verifier; only a live channel
    /// turns that into `locked`.
    ///
    /// The shield must cover the screen *immediately* — a lock that waited on a D-Bus round trip
    /// would leave the desktop readable for as long as gdm took to answer. But it must not claim
    /// to be locked before anything can unlock it, because gdm down, reauth denied and PAM
    /// misconfigured are all silent and all identical from here.
    #[test]
    fn locking_covers_the_screen_first_and_locks_only_once_gdm_answers() {
        let mut shield = ScreenShield::new(ShieldSettings::default());

        let effects = shield.lock(T0, false).expect("not locked down");
        let epoch = effects
            .request_authenticator
            .expect("gdm is asked for a channel");
        assert!(shield.is_active(), "and the screen is covered right away");
        assert!(
            !shield.is_locked(),
            "but not locked until something can unlock it"
        );
        assert_eq!(effects.locked_changed, None);

        let effects = shield.authenticator_ready(epoch, true);
        assert!(shield.is_locked(), "a live channel is what locks it");
        assert_eq!(effects.locked_changed, Some(true));
    }

    /// No channel, no lock — the shield stays a screensaver and any input still raises it.
    #[test]
    fn a_shield_with_no_verifier_never_locks() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let epoch = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();

        let effects = shield.authenticator_ready(epoch, false);
        assert!(shield.is_active(), "the screen stays covered");
        assert!(
            !shield.is_locked(),
            "a shield nothing can unlock must not lock"
        );
        assert_eq!(effects.locked_changed, None);
    }

    /// **The epoch.** A slow channel's answer must not satisfy a *later* lock's gate.
    ///
    /// Opening a channel is a D-Bus round trip that can take a zbus timeout to fail, and a shield
    /// can be dismissed and re-locked inside that window. Both crossings are bad: an old failure
    /// answering a new lock leaves the screen covered but unlocked — any keypress lands on a live
    /// desktop — and an old success answering a new lock locks with no conversation behind it, so
    /// no password can ever be delivered.
    #[test]
    fn a_stale_verifiers_answer_cannot_satisfy_a_later_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());

        let first = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();
        shield.deactivate();
        let second = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();
        assert_ne!(first, second, "each lock asks under its own epoch");

        // The first lock's channel finally fails. It must not consume the second lock's gate.
        assert_eq!(
            shield.authenticator_ready(first, false).locked_changed,
            None
        );
        assert!(!shield.is_locked());

        // ...so the second lock's own answer still locks.
        assert_eq!(
            shield.authenticator_ready(second, true).locked_changed,
            Some(true)
        );
        assert!(shield.is_locked());
    }

    /// If the unlock channel dies, the lock drops to a screensaver rather than becoming a trap.
    #[test]
    fn losing_the_channel_drops_the_lock_rather_than_trapping_the_session() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let epoch = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();
        shield.authenticator_ready(epoch, true);
        assert!(shield.is_locked());

        let effects = shield.authenticator_lost();
        assert_eq!(effects.locked_changed, Some(false));
        assert!(
            !shield.is_locked(),
            "nothing can unlock it, so it must not lock"
        );
        assert!(shield.is_active(), "but the screen stays covered");
    }

    /// A verifier that arrives after the shield was raised must not lock the running session.
    ///
    /// The channel opens asynchronously, so this really happens: lock, dismiss before gdm answers,
    /// then the answer lands. Locking there would drop a lock screen over a desktop the user is
    /// already using.
    #[test]
    fn a_late_verifier_does_not_lock_a_raised_shield() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let epoch = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();
        shield.deactivate();

        let effects = shield.authenticator_ready(epoch, true);
        assert!(!shield.is_active());
        assert!(!shield.is_locked(), "the shield is up; nothing to lock");
        assert_eq!(effects.locked_changed, None);
    }

    /// Going idle covers the screen but does not lock it — the lock is a timer, and the timer has
    /// a ten-second floor even when `lock-delay` is the default zero.
    ///
    /// Getting this wrong in the safe-looking direction (lock immediately) is the bug that makes a
    /// desktop unusable: glance away for the idle delay and you are challenged, with no window to
    /// come back in. `max(STANDARD_FADE_TIME, lock-delay)` is what that window is made of.
    #[test]
    fn going_idle_covers_the_screen_and_arms_a_delayed_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());

        let effects = shield.on_session_idle(T0);
        assert!(effects.start_fade, "idle fades first, it does not cover");
        assert_eq!(effects.active_changed, None, "the shield is still up");
        assert_eq!(effects.arm_lock_timer, Some(STANDARD_FADE_TIME));
        assert!(
            effects.cancel_dialog,
            "no half-typed password left on screen"
        );

        // The fade reaching black is what puts the shield down — and without the slide, since the
        // screen it would slide onto is already black.
        let done = shield.fade_complete(T0 + STANDARD_FADE_TIME);
        assert_eq!(done.active_changed, Some(true));
        assert!(done.curtain_instant);
        assert!(!shield.is_locked(), "covered; the lock timer is what locks");

        // A longer `lock-delay` wins over the floor; a shorter one does not shrink it.
        let mut shield = ScreenShield::new(ShieldSettings {
            lock_delay: Duration::from_secs(60),
            ..Default::default()
        });
        assert_eq!(
            shield.on_session_idle(T0).arm_lock_timer,
            Some(Duration::from_secs(60))
        );
    }

    /// `lock-enabled = false` is a screensaver: idle still covers the screen, but nothing arms.
    #[test]
    fn going_idle_with_locking_off_never_arms_a_lock() {
        let mut shield = ScreenShield::new(ShieldSettings {
            lock_enabled: false,
            ..Default::default()
        });

        let effects = shield.on_session_idle(T0);
        assert!(effects.start_fade);
        assert_eq!(effects.arm_lock_timer, None);
    }

    /// Going idle stamps the activation time even though the shield has not appeared yet.
    ///
    /// `GetActiveTime` is what gnome-session and gsd read to decide how long the seat has been
    /// unattended (`:257-258`). Stamping it when the shield finally covers, ten seconds later,
    /// under-reports every idle screensaver by exactly that fade.
    #[test]
    fn going_idle_stamps_the_activation_time_before_the_fade() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.on_session_idle(T0);
        assert_eq!(shield.activation_time(), Some(T0));
        assert!(!shield.is_active(), "and it is not covered yet");
        assert_eq!(shield.active_time_secs(T0 + Duration::from_secs(30)), 30);
    }

    /// A screensaver that is already covering the screen still gets locked when the session goes
    /// idle.
    ///
    /// GNOME's only guard here is the fade already running (`:248-251`); the arming condition is
    /// `lockEnabled && !isLocked` and says nothing about whether the shield is down (`:258`). An
    /// `active`-based guard looks like the obvious idempotence fix and is a security hole: raise a
    /// screensaver by hand (or lose a lock to `authenticator_lost`), walk away, and with
    /// `lock-enabled = true` the screen stays covered and unlocked forever.
    #[test]
    fn an_already_covered_screensaver_still_locks_on_idle() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.activate(T0);

        let effects = shield.on_session_idle(T0 + Duration::from_secs(5));
        assert_eq!(effects.active_changed, None, "already covered");
        assert_eq!(
            effects.arm_lock_timer,
            Some(STANDARD_FADE_TIME),
            "but the lock is still owed"
        );
    }

    /// A lock already in flight is not re-armed — that is the real analogue of GNOME's
    /// fade-in-progress guard.
    #[test]
    fn idling_while_a_lock_is_in_flight_does_not_re_arm() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.on_session_idle(T0);
        // The timer fired: the shield asked gdm for a channel and is waiting on the answer.
        shield.lock(T0, false).expect("not locked down");

        let again = shield.on_session_idle(T0 + Duration::from_secs(5));
        assert_eq!(again.arm_lock_timer, None);
    }

    /// A shield waiting on its verifier cannot be dismissed by input.
    ///
    /// GNOME's `lock()` sets `_isLocked` synchronously (`:660`), so it has no such window. Ours
    /// gates on a live gdm channel, and treating the wait as a screensaver would mean a lock is
    /// beaten by whoever presses a key first — walk up to a machine that just suspended, wiggle
    /// the mouse, and you are past it.
    #[test]
    fn a_lock_waiting_on_its_verifier_cannot_be_wiggled_away() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let epoch = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();

        assert!(!shield.is_dismissible(), "the answer has not landed yet");
        assert_eq!(
            shield.on_user_active().active_changed,
            None,
            "input does not raise it"
        );
        assert!(shield.is_active(), "still covering the screen");
        assert!(
            shield.wants_sleep_inhibitor(true),
            "and the suspend still has to wait for us"
        );

        // A refused channel ends the wait: there is nothing to authenticate against, so the shield
        // goes back to being the screensaver it is entitled to be.
        shield.authenticator_ready(epoch, false);
        assert!(shield.is_dismissible());
    }

    /// Resuming raises a shield that was only a screensaver (`_wakeUpScreen` → `:499`).
    #[test]
    fn resuming_raises_a_screensaver_but_not_a_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.activate(T0);

        let effects = shield.wake_up_screen();
        assert!(effects.wake_up_screen);
        assert!(!shield.is_active(), "nothing to authenticate; it goes away");

        let mut shield = ScreenShield::new(ShieldSettings::default());
        let epoch = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();
        shield.authenticator_ready(epoch, true);
        shield.wake_up_screen();
        assert!(shield.is_active(), "a lock survives the resume");
    }

    /// Coming back during the grace period takes the screensaver *and* the pending lock away.
    ///
    /// If the timer survived the deactivate, the desktop would lock itself moments after the user
    /// dismissed the screensaver and started working.
    #[test]
    fn coming_back_before_the_timer_cancels_the_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        shield.on_session_idle(T0);
        shield.fade_complete(T0 + STANDARD_FADE_TIME);

        let effects = shield.on_user_active();
        assert_eq!(effects.active_changed, Some(false));
        assert!(effects.cancel_lock_timer);
        assert!(effects.stop_fade);

        // Once locked, activity is for the unlock dialog to handle; the shield stays put.
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let epoch = shield
            .lock(T0, false)
            .expect("not locked down")
            .request_authenticator
            .unwrap();
        shield.authenticator_ready(epoch, true);
        let effects = shield.on_user_active();
        assert_eq!(effects.active_changed, None, "a locked shield stays put");
        assert!(shield.is_locked());
    }

    /// Coming back *during the fade*, before anything has covered the screen, cancels the lock too.
    ///
    /// The nastier half of the case above, and the one worth its own test: for those ten seconds
    /// the shield is not active, so a "is there anything to dismiss?" guard says no and returns
    /// early — leaving the armed lock running. The user waves the mouse, the fade disappears, they
    /// go back to work, and the machine locks under them seconds later with no warning at all.
    #[test]
    fn coming_back_during_the_fade_cancels_the_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        let idle = shield.on_session_idle(T0);
        assert!(idle.start_fade);
        assert!(idle.arm_lock_timer.is_some());
        assert!(!shield.is_active(), "the fade has not reached black yet");

        let effects = shield.on_user_active();
        assert!(effects.stop_fade, "the fade goes away");
        assert!(
            effects.cancel_lock_timer,
            "and the armed lock with it, or the desktop locks under the user"
        );
        assert!(
            shield.activation_time().is_none(),
            "the seat is attended again, so the idle clock restarts"
        );
    }

    /// Suspending locks straight away — a grace period on a machine that is going to sleep would
    /// be a machine that suspends unlocked.
    #[test]
    fn suspending_locks_without_a_grace_period() {
        let mut shield = ScreenShield::new(ShieldSettings::default());

        let effects = shield.prepare_for_sleep(true, T0, false);
        assert_eq!(effects.active_changed, Some(true));
        assert!(effects.request_authenticator.is_some(), "asks to lock now");
        assert_eq!(effects.arm_lock_timer, None);
        assert!(effects.clear_clipboard);

        // Resuming only wakes the screen.
        let effects = shield.prepare_for_sleep(false, T0, false);
        assert!(effects.wake_up_screen);
        assert!(shield.is_active(), "resume does not raise the shield");
    }

    /// `lock-enabled = false` means suspend leaves the session as it was.
    #[test]
    fn suspending_with_locking_off_does_not_cover_the_screen() {
        let mut shield = ScreenShield::new(ShieldSettings {
            lock_enabled: false,
            ..Default::default()
        });

        let effects = shield.prepare_for_sleep(true, T0, false);
        assert_eq!(effects.active_changed, None);
        assert!(!shield.is_active());
    }

    /// The sleep inhibitor is held exactly while a suspend would still owe a lock.
    ///
    /// Holding it forever would delay every suspend for nothing; never holding it means
    /// `PrepareForSleep` arrives with no time to act, which is the same as not locking at all.
    #[test]
    fn the_sleep_inhibitor_is_held_only_while_a_suspend_would_owe_a_lock() {
        let mut shield = ScreenShield::new(ShieldSettings::default());
        assert!(shield.wants_sleep_inhibitor(true));

        assert!(
            !shield.wants_sleep_inhibitor(false),
            "an inactive VT must not hold up anyone else's suspend"
        );

        shield.activate(T0);
        assert!(
            !shield.wants_sleep_inhibitor(true),
            "already covered; nothing left to do before sleeping"
        );

        shield.deactivate();
        for settings in [
            ShieldSettings {
                lock_enabled: false,
                ..Default::default()
            },
            ShieldSettings {
                disable_lock_screen: true,
                ..Default::default()
            },
        ] {
            shield.set_settings(settings);
            assert!(!shield.wants_sleep_inhibitor(true));
        }
    }
}
