# Session lock and lock screen — porting `ScreenShield`

Status: **slices 1–3 landed 2026-08-01** (the model + the D-Bus surface; the curtain; the unlock
dialog and gdm authentication). Slices 4–5 unstarted.
Cited against `js/ui/screenShield.js` (675 lines), `js/ui/unlockDialog.js` (972) and
`js/ui/shellDBus.js:517-566` in the 50.3 checkout.

## Why this came up

The D-Bus audit (`dbus-surface-audit.md` §1) found the session could not lock at all. We served
`org.freedesktop.ScreenSaver`, which is only `Inhibit`/`UnInhibit` — what a video player calls to
stop blanking. Nothing in it locks. The locking interface is `org.gnome.ScreenSaver`, and it was
unowned, so gsd-power's idle lock, its lock-on-suspend, and `loginctl lock-session` all landed on
a name nobody answered.

## The shape in GNOME

**The shell is the lock screen.** There is no external locker in a GNOME session, which is why
`ext-session-lock` (inherited from niri, for swaylock and friends) does not substitute: nothing in
the session speaks it. Per the fork tenet that is niri's way, kept only as an extra capability.

**Two booleans, not one** (`_setActive` `:156-164`, `_setLocked` `:166-175`):

- `active` — the shield is down. This is the screensaver.
- `locked` — getting back in needs authentication.

`active && !locked` is a real state, not a transient: it is a blanked screen with
`org.gnome.desktop.screensaver lock-enabled = false`, and it is what a user whose AccountsService
`password_mode` is NONE always gets (`lock`, `:637-661`). Collapse the two and you either demand a
password from someone who has none, or blank without ever locking.

**Names and objects.** gnome-shell exports the object on `org.gnome.Shell.ScreenShield` at
`/org/gnome/ScreenSaver`, and ships a separate gjs service owning `org.gnome.ScreenSaver` that
proxies to it (`js/dbusServices/screensaver/`). The split makes the well-known name activatable
while the shell is down. We have no such staging, so we own both names on one connection — but the
object path is the same, because that is what callers ask for.

**Auth is out of process.** `unlockDialog.js` uses `Gdm.Client`'s reauthentication channel
(`../gdm/authPrompt.js`), i.e. gdm's `UserVerifier` over D-Bus, which runs PAM in gdm's worker. So
the faithful port needs no PAM in the compositor — which is also what
`untrusted-content-process-seam` would ask for independently. gdm and AccountsService are both
live on this machine, so the path is available.

## Slices

1. **The model + the D-Bus surface.** `ScreenShield` state machine, `org.gnome.ScreenSaver` on
   both names, `ActiveChanged` / `WakeUpScreen`, the lockdown and `lock-enabled` settings, the
   clipboard wipe, logind's `LockedHint`. **Landed.**
2. **The shield UI** — the curtain: wallpaper, clock, date, and the keypress/click that raises it.
   **Landed** (`src/ui/lock_screen.rs`). Notifications on the lock screen are *not* in it; they
   need the message list re-homed onto the shield and are their own piece of work.
3. **The unlock dialog + gdm auth** — the password entry, the avatar and user name, error and
   retry states, `OpenReauthenticationChannel` and the `UserVerifier` conversation. **Landed**
   (`src/dbus/gdm.rs`, `src/unlock_dialog.rs`, the prompt page in `src/ui/lock_screen.rs`).
4. **Integration** — idle → `lock-delay`, and `PrepareForSleep`. **Landed**
   (`src/dbus/gnome_session_presence.rs`, the sleep handling in `src/dbus/freedesktop_login1.rs`).
   logind's session `Lock`/`Unlock` **landed early** with slice 3: without `Unlock`, authenticating
   at gdm's own login screen switches the VT back to a session that is still locked, which is a
   stuck session rather than a missing feature. `lockIfWasLocked` was **dropped** — see below.
5. **The look** — deferred deliberately until the shield *works*, at Gustavo's call (2026-08-01,
   after seat-validating slice 2). The clock↔prompt crossfade has **landed**
   (`PageTransform` in `src/ui/lock_screen.rs`); the blur and the shield's own slide are open:
   - the **blur**. `BLUR_RADIUS = 90` over the wallpaper (`unlockDialog.js:35`), paired with the
     `BLUR_BRIGHTNESS` already shipped. Wanted beyond the lock screen — a reusable blurred-backdrop
     pass is its own toolkit verb, not a lock-screen detail, so build it as one.
   - the **animations**: the shield's rise and fall — `translation_y` between `-screen_height` and
     0 over `Overview.ANIMATION_TIME` (250 ms, `EASE_OUT_QUAD`; `_resetLockScreen` `:452-462`,
     `_continueDeactivate` `:551-556`) — and the idle path's 10 s fade to black.

The **crossfade** is not a dissolve. The clock leaves upward while the prompt arrives from below,
both shrinking to `FADE_OUT_SCALE` and scaling about their own centres (`pivot_point(0.5, 0.5)`,
`:599`, `:604`); it is that opposition that reads as one page giving way to the other. Mid-fade both
pages are on screen with their own alpha, scale and offset, so the render path draws both.

Two traps it walked into, worth keeping:

- The scale rides the **buffer scale**, not a re-bake — a 300 ms fade that re-rasterizes its text
  every frame is the [[animation-per-frame-bake]] shape. Icons have no buffer-scale knob, so their
  scale is bucketed to 16 steps that populate the cache once.
- A half-finished crossfade draws the incoming page at a *partial alpha*, so anything sampling the
  screen right after a page change sees a nearly-invisible prompt and reads it as "the prompt did
  not draw". That cost one Vulkan render test; `LockScreen::settle_page` is the way out.

## The lock gate

`ScreenShield::lock` does **not** set `locked`. It puts the shield down at once — a lock must not
wait on a D-Bus round trip to cover the screen — and asks for a verifier; only
`authenticator_ready(epoch, true)`, driven by a genuinely open gdm channel, turns that into
`locked`. A shield with no verifier stays a screensaver: covered, and raised by any input.

This replaced a static `can_authenticate` flag, and the reason is that the failure modes are
invisible from inside the compositor. gdm not running, the reauthentication channel denied, PAM
misconfigured — all silent, all identical, all producing a screen that cannot be unlocked. A gate
that is *a live conversation* cannot be wrong about that; a flag someone remembered to set can.

Two hazards it closes, both pinned by tests in `screen_shield.rs`:

- **The epoch.** Opening a channel can outlive the lock that asked for it (lock, dismiss, re-lock).
  An old *failure* answering a new lock leaves the screen covered but unlocked; an old *success*
  locks with no conversation behind it. Each lock asks under its own epoch and ignores the others.
- **A lost channel.** If gdm goes away mid-conversation the lock is a trap — `answer_query` no-ops
  on a dead conversation *after* replying successfully, so the user sees no error and makes no
  progress. `authenticator_lost` drops the lock back to a screensaver. This is a divergence: GNOME
  leaves the dialog stuck. It is not a weakening, since killing gdm needs root and root can already
  read the session.

**First seat test: open a second VT before typing a wrong password.**

## Going idle, and going to sleep

Two ways in besides someone asking, and they want opposite things.

**Idle is patient.** The threshold is *not ours*: `org.gnome.desktop.session idle-delay` belongs to
gnome-session, which watches the seat through mutter's `IdleMonitor` — the one we already serve —
and publishes its verdict on `org.gnome.SessionManager.Presence`. The shell only listens
(`screenShield.js:78-88`). Reimplementing the threshold against our own idle monitor would be less
machinery and worse: gsd-power, the presence indicator and everything else honouring idleness would
go idle at a moment the screen did not, and `idle-delay = 0` would stop meaning "never".

On IDLE the screen is covered and a timer is armed for `max(STANDARD_FADE_TIME, lock-delay)` —
**ten seconds even when `lock-delay` is the default zero**, because the 10 s fade is a floor, not
just an animation. Coming back cancels the timer, and that cancellation is the whole feature: a
screensaver you dismiss must not lock you out a moment later.

**The gate's window is not a screensaver.** GNOME's `lock()` sets `_isLocked` synchronously
(`:660`); ours cannot, because the gate is a live gdm channel and opening one is a round trip. So
between `lock` and `authenticator_ready` the shield is down with `locked` still false, and treating
that as a screensaver would mean a lock is beaten by whoever presses a key first — suspend, walk up,
wiggle the mouse. `is_dismissible()` is what closes it: input raises the shield only when it is
active, not locked, *and* not waiting on an answer. The sleep inhibitor is held across the same
window for the same reason (logind's `InhibitDelayMaxSec` bounds how long that can hold anyone up).
A refused channel ends the wait and the shield goes back to being the screensaver it is entitled to
be.

Which makes *something always answering* load-bearing, because an unanswerable request is a worse
lockout than the lock it stood in for — covered, unlockable and unraisable. Two ways it would not
be answered, both closed: nobody to ask (no D-Bus, a gdm client that failed to start, or any
non-session instance) is answered on the spot; a gdm that takes the request and goes quiet is
answered by a 10 s watchdog. A dead socket already arrived as `Lost`. All three land on the same
epoch-tagged `authenticator_ready(_, false)`, so an answer for an abandoned lock cannot refuse a
later one.

**Sleep is not patient.** `PrepareForSleep(true)` locks immediately, no grace period — a machine
about to suspend must not suspend unlocked. This only works because of the `delay` sleep inhibitor
(`_syncInhibitor`, `:202-231`): holding that fd is what makes logind emit the signal and *wait* for
us. It is held exactly while a future suspend would still owe a lock — not while already covered,
not on a background VT, not when locking is off — and dropping it is how we say "go ahead". Its
absence is not cosmetic: it silently turns the suspend lock into a race.

**`lockIfWasLocked` is deliberately not ported.** It reads a runtime-state key that nothing writes
any more: the write was X11-only and went away with the X11 backend (`71b19fa42`, Nov 2025), whose
own comment is the reason — *"On wayland, a crash brings down the entire session, so we don't need
to defend against being restarted unlocked."* We are Wayland-only, so porting it would be porting
a function that always returns early.

## `.../session/auto` is not our session

Getting gdm's login screen to unlock us needed logind's `Session.Unlock`, and the first attempt
subscribed on `/org/freedesktop/login1/session/auto`. It never fired. Two separate reasons, and the
second one had been quietly breaking things since long before the lock screen:

- **Signals are not emitted on `auto`.** It is a per-caller alias logind resolves from the sender's
  pid when a message is *addressed* to it; no object lives there. The session broadcasts from its
  escaped concrete path — session `116` is `/org/freedesktop/login1/session/_3116`. A match rule on
  `auto` subscribes successfully and then stays empty forever, which is the worst failure shape:
  no error anywhere.
- **`auto` does not resolve for us at all.** A GNOME session runs the shell as a *user service*
  (`user@1002.service/session.slice/org.gnome.Shell@user.service`), outside the session scope, so
  logind answers `NoSessionForPID`. Every `Session` call on `auto` from the compositor fails —
  which is what `set_brightness` had been doing.

`freedesktop_login1::resolve_session_path` now resolves it once, at startup: `GetSessionByPID` for
our own pid (right when we *are* in a session scope, e.g. started from a TTY), falling back to the
`Display` property of `/org/freedesktop/login1/user/_<uid>` — logind's own "this user's graphical
session" — which is the answer for a user service. Both hand back the escaped path, so we never
reimplement systemd's `bus_label_escape`. `SetLockedHint` was resolving separately via
`XDG_SESSION_ID`, an env var a user service need not carry; it goes through the same path now.

gdm's side is `session_unlock` in `daemon/gdm-manager.c`, which calls logind's
`Manager.UnlockSession` — so authenticating at gdm really does arrive as this signal, and
`loginctl unlock-session <id>` is the same thing by hand, which is how this was validated.

## The honest password exposure

Authentication runs in gdm's PAM worker, not here, but the plaintext does pass through this
process. What is done about it, and what is not:

- The entry buffer is pre-sized (`ENTRY_CAPACITY`) so typing never reallocates — a `String` growing
  0→8→16 strands unzeroed prefixes on the heap — and `clear_entry` volatile-zeroes before
  truncating. Every clear path goes through it.
- `VerifierRequest`'s `Debug` is hand-written to print `Answer(<redacted, n chars>)`. A derived one
  would put the password in the journal from any `{:?}` in a wrapper, and several types embed it.
- **Not** addressed: zbus copies the answer into a message body buffer on the way to the socket and
  drops it unzeroed; there is no `mlock`, so the buffer can be swapped; and `RLIMIT_CORE` is not
  suppressed, so a compositor crash during authentication can write it into a core dump. These need
  either a zbus change or process-wide policy, and are listed here rather than left implied.

## Known divergences

- **`Lock` replies immediately.** GNOME defers the D-Bus reply until `lock-screen-shown`
  (`shellDBus.js:538-546`), so a caller that locks and then suspends cannot race the shield onto
  the screen. The curtain now exists, so there *is* something to wait for — the remaining piece is
  a "first frame with the shield up has been presented" signal to hang the reply on. Still open.
- **No blur, and no animation.** Both are slice 5 above, by decision rather than oversight; only
  `BLUR_BRIGHTNESS = 0.65` ships, as a black wash over the wallpaper. It is the half that makes the
  white 72pt clock legible over an arbitrary picture.
- **The clock weight is 700, not 800.** Our rasterizer's ceiling, not a decision here — the same
  standing divergence as every other `%title_1` in the port.
- **Touch mode is assumed off**, so the hint always reads "Click or press a key to unlock" rather
  than "Swipe up to unlock". The seat's touch-mode property is not tracked yet.
- **Both idle watches are our own.** GNOME hangs the hint's 4 s and the prompt's 2 min escape on
  the *core idle monitor* (whole-seat idleness); we measure time since the last interaction with the
  shield. Equivalent while the shield swallows all input. The prompt's escape also rides the panel's
  minute tick, so it is granular to a minute rather than exact — the right trade for a timeout whose
  only job is to not leave a half-typed password on an unattended screen.
- **The idle escape does not cancel the conversation.** GNOME's `_escape` calls `authPrompt.cancel()`
  (`unlockDialog.js:862-865`), tearing the verifier down; ours only clears the entry and flips back
  to the clock, keeping the channel so a user who wanders back finds gdm still waiting. Fail-closed
  either way — ours just holds the channel longer.
- **`password_mode` is assumed to be "has a password."** AccountsService is not read yet, so
  `lock` is told `password_mode_none = false`. Conservative: a passwordless account gets a shield
  that asks gdm for a channel, and gdm refusing is what keeps it a screensaver.
- **The user comes from the passwd entry, not AccountsService** — login name, and GECOS as the real
  name, which is what AccountsService seeds `real-name` from. Diverges only for an account renamed
  through AccountsService without touching GECOS. The avatar is always the themed
  `avatar-default-symbolic`; the per-user icon file is not read.
- **`SetActive(false)` unlocks without authenticating.** Any session-bus client can raise a locked
  shield. This is GNOME's own behaviour (`shellDBus.js:551-552` → `screenShield.js:515-519` →
  `_setLocked(false)`, with no verification check), inherited deliberately — do not "fix" it
  without changing GNOME too.
- **The message block is centred, its wrapped lines are not.** `text-align: center`
  (`_login-lock.scss:90-92`) centres each line; our paragraph shaper centres the block and
  left-aligns within it. Invisible at one line, which is every message PAM actually sends.
- **The peek toggle is in.** `view-reveal-symbolic` / `view-conceal-symbolic` at the entry's
  trailing edge (`st-password-entry.c:223-230`, `:333-346`), gated on
  `org.gnome.desktop.lockdown disable-show-password` and dropped on every fresh question.
- **No caps-lock warning** (`authPrompt.js:414`). Worth having: it is the most common cause of the
  wrong-password loop.
- **Only `gdm-password`.** Fingerprint and smartcard are separate PAM services GNOME starts in
  parallel on the same channel (`util.js:709-719`); adding them is additive — start the service and
  route by `service_name` — but GNOME also rewrites fingerprint `Info` into its own hint text
  (`util.js:727-732`) and counts its `Problem`s against `allowed-failures` (`:745-775`), so it is
  not merely a second subscription.
- **The idle path does not fade.** GNOME raises a black lightbox over the desktop and only puts the
  shield down when that 10 s fade completes; we cover the screen at once. The grace period before
  *locking* is the same length either way — it just looks like the curtain instead of a dimming
  desktop. The fade is slice 5's, with the other animations.
- **User-active comes from presence, not the idle monitor.** GNOME cancels the pending lock from
  the core idle monitor's `add_user_active_watch` (`:282`), which fires on real input; ours rides
  gnome-session's next `Available` status. Real input already cancels it anyway, because it raises
  an unlocked shield through `on_shield_key`. Only `Available` counts — `Busy` and `Invisible` are
  app- and user-set presence, not activity, and treating them as a return would un-blank an
  unattended screen. Hanging this on our own `IdleMonitor` (which we already serve) is the proper
  fix and is not done.
