# Session lock and lock screen — porting `ScreenShield`

Status: **slices 1–5 landed 2026-08-01** — the model + the D-Bus surface; the curtain; the unlock
dialog and gdm authentication; idle and suspend; and the look (blur, crossfade, the shield's slide,
the idle fade to black).
Cited against `js/ui/screenShield.js` (675 lines), `js/ui/unlockDialog.js` (972) and
`js/ui/shellDBus.js:517-566` in the 50.3 checkout.

**What is still missing** is scoped item by item in `lock-screen-backlog.md`, with the reference
citations for each.

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
   after seat-validating slice 2). **Landed**, in three pieces:
   - the **blur** (`synoik-vk/src/blur.rs`, `render_helpers/vulkan/gaussian_backdrop.rs`,
     `Wallpaper::render_blurred`);
   - the **clock↔prompt crossfade** (`PageTransform` in `src/ui/lock_screen.rs`);
   - the **shield's own slide** and the **idle fade to black** (`Curtain` and `fade_alpha`, same
     file).

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

## The blurred backdrop

GNOME puts a `Shell.BlurEffect` on the lock screen's *background actor* — not on the framebuffer —
with `BLUR_RADIUS = 90` and `BLUR_BRIGHTNESS = 0.65` (`unlockDialog.js:706-713`, `:34-35`), so the
brightness rides inside the blur rather than being a wash laid over it.

Its blur is a separable gaussian with `sigma = radius / 2`, run on a downscaled copy — the same
kernel the whole renderer uses. The chain (`synoik-vk/src/blur.rs`) is gaussian-only: there is no
Kawase shader and every entry point on it is a gaussian one, so the window background effect runs
this too. The pyramid, render pass and sampler are shared; only the parameter differs, a radius in
pixels rather than the passes and tap offset the inherited chain took, which had no way to say
"90 pixels".

Three things worth keeping:

- **The downscale cascade is the whole trick.** Radius 90 on 1080p lands on a 240x135 buffer where
  sigma is 5.6 and the shader runs 19 taps per direction instead of 271. GNOME cascades two
  downscales (`ShellBlurEffect`'s, then `ClutterBlur`'s), but they collapse: the first stops once
  `radius / f <= 12`, which is exactly `sigma <= 6`, which is the second's own threshold.
- **`BLUR_RADIUS` is in stage pixels, and the wallpaper is not.** The picture is stored at its own
  resolution and scaled to the screen when drawn, so blurring it with a screen-space radius makes a
  4K picture on a 1080p output come out half as blurred as GNOME's. `render_blurred` converts by
  the magnification the draw will apply.
- **The blur is queued, never submitted.** It runs during element building, where no command buffer
  is open; `VulkanRenderer::queue_gaussian_blur` hands it to the next frame's, as the older path
  already did (`docs/fork/frame-submit-discipline.md`). It also only re-runs when the wallpaper,
  radius or brightness actually change — a lock screen redraws on every clock tick and keystroke.

## The slide, and the fade

The shield's rise and fall is `translation_y` between `-screen_height` and 0 over
`Overview.ANIMATION_TIME` (250 ms, `EASE_OUT_QUAD`; `_resetLockScreen` `:452-462`,
`_continueDeactivate` `:551-556`). The idle path gets the other one: a black lightbox faded in over
`STANDARD_FADE_TIME` (10 s, `_activateFade` `:275-283`), whose *completion* — not its start — is
what activates the shield (`_onLongLightbox` `:311-314`).

Four things worth keeping:

- **Idle does not put the shield down.** `on_session_idle` starts the fade and stamps
  `_activationTime`, and `fade_complete` is what activates. Covering the screen the moment the
  session is declared idle is the obvious shortcut and it costs the user both the gradual warning
  and the ten seconds they have to wave the mouse and cancel it. The stamp still goes at the
  *start*, because `GetActiveTime` is what gnome-session and gsd read to decide how long the seat
  has been unattended, and stamping at the fade's end under-reports every screensaver by 10 s.
- **Coming back mid-fade has to cancel the armed lock.** For those ten seconds nothing is active
  yet, so a "is there anything to dismiss?" guard answers *no* and returns early — leaving the lock
  timer running, so the user goes back to work and the machine locks under them seconds later.
  `on_user_active` therefore gates on `is_challenging()` (locked, or awaiting a verifier), not on
  `is_dismissible()` (which also requires the shield to be down). Pinned by
  `coming_back_during_the_fade_cancels_the_lock`.
- **The idle path's curtain does not slide.** By the time the shield goes down the screen is
  already black, so a slide would animate a picture nobody can see; `ShieldEffects::curtain_instant`
  is that case. The curtain's `is_covering` is read off the `Curtain` *state*, never off its
  progress float — at the exact instant a descent begins the float is 0, and trusting it shows one
  frame of desktop under a locked session.
- **A slide-out still owes the vacated band.** The shield render branch cannot `return` early once
  it has drawn the shield: during the fall the strip it has left is its own to paint. Same shape as
  the crossfade's trap above — and both animations start from invisible, so a Vulkan render test
  taken right after the state change catches nothing. `LockScreen::settle()` is the way out.

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

- **No "beat", and no second fade to black.** GNOME dims the screen 300 ms after a manual lock
  lands and defers `ActiveChanged` to the end of that dim, so gnome-settings-daemon does not blank
  mid-animation (`screenShield.js:479-486`, `:316-319`, `:604-614`). We keep the reason and drop
  the mechanism: blanking is power management's business, not the lock screen's. The published
  `active` simply waits for the curtain to land — rises wait, falls are immediate — which is all
  the lock screen needs to guarantee its animation is seen. Decided 2026-08-01; see
  `lock-screen-backlog.md` item H.

- **`Lock`'s reply is level-triggered, not edge-triggered.** GNOME hangs `LockAsync` on the
  `lock-screen-shown` *edge* (`shellDBus.js:538-545`), so a `Lock` at an already-covered screen
  waits forever — `_resetLockScreen` returns early unless the shield is hidden
  (`screenShield.js:440-445`) and never emits again — and a lockdown-refused `Lock` never reaches
  the emit at all. We ask "is the curtain down?" instead, which answers both immediately.
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
- **The caps-lock row is always reserved.** Note the cost lands on the prompt everyone sees: on a
  *secret* question with caps off GNOME reserves nothing at all (the placeholder's visibility is
  bound inverse to the warning, and the warning's own height eases to 0), so our password prompt
  carries one extra line box plus its spacing that GNOME's does not. GNOME eases the warning's *height* from 0
  (`shellEntry.js:210-217`) and holds the line with an empty placeholder on non-secret questions
  (`authPrompt.js:201-212`); we reserve the row always and fade only the text. Animating the height
  would move everything below it every frame, which re-rasterises the column every frame. The cost
  is one blank line under a password entry with caps off — the same space GNOME's own placeholder
  reserves on a non-secret prompt.
- **Only `gdm-password`.** Fingerprint and smartcard are separate PAM services GNOME starts in
  parallel on the same channel (`util.js:709-719`); adding them is additive — start the service and
  route by `service_name` — but GNOME also rewrites fingerprint `Info` into its own hint text
  (`util.js:727-732`) and counts its `Problem`s against `allowed-failures` (`:745-775`), so it is
  not merely a second subscription.
- **User-active comes from presence, not the idle monitor.** GNOME cancels the pending lock from
  the core idle monitor's `add_user_active_watch` (`:282`), which fires on real input; ours rides
  gnome-session's next `Available` status. Real input already cancels it anyway, because it raises
  an unlocked shield through `on_shield_key`. Only `Available` counts — `Busy` and `Invisible` are
  app- and user-set presence, not activity, and treating them as a return would un-blank an
  unattended screen. Hanging this on our own `IdleMonitor` (which we already serve) is the proper
  fix and is not done.
