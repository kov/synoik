# Session lock and lock screen — porting `ScreenShield`

Status: **slices 1–2 landed 2026-08-01** (the model + the D-Bus surface; the curtain). Slices 3–4
unstarted.
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
   retry states, `OpenReauthenticationChannel` and the `UserVerifier` conversation. **This is the
   slice that makes `locked` safe to enter.**
4. **Integration** — idle → `lock-delay`, logind `Lock`/`Unlock` signals and `PrepareForSleep`,
   `lockIfWasLocked` after a crash (`:663-674`).

## The safety rail (slice 1 → 3)

`ScreenShield::lock` **does not set `locked`**, and holds a `can_authenticate` flag that is false
until slice 3. This is a deliberate divergence with a one-line justification: entering `locked`
with no unlock dialog is a lockout — the shield covers the screen and nothing short of a VT switch
gets you back. A lock that traps whoever tries it is worse than no lock.

So slice 1 is honest about what it ships: `Lock` blanks (once slice 2 draws anything), clears the
clipboard, reports `GetActive` truthfully and emits `ActiveChanged`. It does not claim to be
secure, and `LockedHint` stays false, so nothing downstream is misled either.

`screen_shield.rs`'s `locking_without_an_unlock_path_activates_but_does_not_lock` is the test that
pins this, and it is where slice 3 flips the flag.

**Do not enable `can_authenticate` before the unlock dialog is live on a seat you can still reach.**

## Known divergences

- **`Lock` replies immediately.** GNOME defers the D-Bus reply until `lock-screen-shown`
  (`shellDBus.js:538-546`), so a caller that locks and then suspends cannot race the shield onto
  the screen. The curtain now exists, so there *is* something to wait for — the remaining piece is
  a "first frame with the shield up has been presented" signal to hang the reply on. Still open.
- **No blur.** `BLUR_RADIUS = 90` is not implemented; only `BLUR_BRIGHTNESS = 0.65` is, as a black
  wash over the wallpaper. Cosmetic, and the brightness is the half that makes the white 72pt clock
  legible over an arbitrary picture.
- **The clock weight is 700, not 800.** Our rasterizer's ceiling, not a decision here — the same
  standing divergence as every other `%title_1` in the port.
- **Touch mode is assumed off**, so the hint always reads "Click or press a key to unlock" rather
  than "Swipe up to unlock". The seat's touch-mode property is not tracked yet.
- **The hint's idle watch is our own.** GNOME hangs it on the core idle monitor; we restart it from
  activation and from input on the shield. Equivalent while the curtain swallows all input, and it
  is the line that changes when slice 3 gives the prompt something to keep alive.
- **`password_mode` is assumed to be "has a password."** AccountsService is not read yet, so
  `lock` is told `password_mode_none = false`. Conservative, and inert while the safety rail
  holds.
- **`lock-delay`** (the grace period between blanking and locking) is not modelled; it belongs
  with slice 4's idle integration.
