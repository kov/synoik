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
4. **Integration** — idle → `lock-delay` and `PrepareForSleep`, `lockIfWasLocked` after a crash
   (`:663-674`). logind's session `Lock`/`Unlock` **landed early** with slice 3: without `Unlock`,
   authenticating at gdm's own login screen switches the VT back to a session that is still
   locked, which is a stuck session rather than a missing feature.
5. **The look** — deferred deliberately until the shield *works*, at Gustavo's call (2026-08-01,
   after seat-validating slice 2):
   - the **blur**. `BLUR_RADIUS = 90` over the wallpaper (`unlockDialog.js:35`), paired with the
     `BLUR_BRIGHTNESS` already shipped. Wanted beyond the lock screen — a reusable blurred-backdrop
     pass is its own toolkit verb, not a lock-screen detail, so build it as one.
   - the **animations**: the shield's rise and fall, and the clock↔prompt crossfade
     (`CROSSFADE_TIME`, `FADE_OUT_TRANSLATION = 200`, `FADE_OUT_SCALE = 0.3`).

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
- **`lock-delay`** (the grace period between blanking and locking) is not modelled; it belongs
  with slice 4's idle integration.
