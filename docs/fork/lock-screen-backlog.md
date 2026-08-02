# Lock screen — the remaining work

The five porting slices in `lock-screen-port.md` are landed: the session locks, authenticates,
unlocks, survives suspend, and animates. What follows is the list of things GNOME's lock screen does
that ours still does not, scoped against the 50.3 checkout at `~/Projects/gnome-shell`.

Most items here are a bullet under **Known divergences** in `lock-screen-port.md`; as each lands,
delete it from that list. (Two are not: **D** is only mentioned in slice 2's prose, and **H** was
missed by both documents until a review caught it.) The divergence list is the source of truth for
what is *accepted*, this file for what is *planned*.

Ordered by what I would do first, which is mostly about size rather than dependency. The only real
dependency is **H on B** — they touch the same moment. D depends on our notification subsystem,
which is done; it does **not** depend on C, despite sharing a layout with C's switch-user button.

Citations below saying `util.js` mean `js/gdm/util.js`, not `js/misc/util.js`, which also exists.

**All of this is animated, and animations here are a known flake source.** The caps warning eases
over 200 ms, the notification rows over their own height, the crossfade over 300 ms. Every test
touching them settles the animation first — see `headless-animation-clock-trap`.

---

## A. Caps-lock warning — **LANDED**

**Why first:** smallest thing on the list, and it is the most common cause of the wrong-password
loop — the user types the right password three times and learns nothing.

`ShellEntry.CapsLockWarning` (`js/ui/shellEntry.js:162-218`) is an `St.Label` reading
**"Caps lock is on"**, style class `caps-lock-warning-label`, coloured `$_gdm_fg`
(`_login-lock.scss:10-13`) — the same colour as the rest of the dialog text, not a warning red.
Ellipsis off, line wrap on (`:172-173`).

Four details that are easy to miss:

- **It is only shown for `secret` questions.** `this._capsLockWarningLabel.visible = secret`
  (`authPrompt.js:414`), set from `_updateEntry`. A username prompt gets no warning.
- **A placeholder holds its space.** An empty `St.Label` is added to the input well and bound to the
  warning's `visible` with `INVERT_BOOLEAN` (`authPrompt.js:201-212`). It equalises the *secret vs
  non-secret* case — the row keeps its height when a username question replaces a password one. A
  caps-lock toggle still animates the height; that is the 200 ms ease, and it is deliberate.
- **It animates**: height and opacity, 200 ms (`shellEntry.js:210-217`), with `height = -1` restored
  on completion so the label goes back to sizing itself.
- **The state comes from the keymap, not from keystrokes**: `seat.get_keymap()` plus a
  `state-changed` subscription (`shellEntry.js:175-188`).

**Our side.** `ModifiersState::caps_lock` already exists (smithay
`input/keyboard/modifiers_state.rs:22`) and already reaches `on_shield_key`. Caps Lock is in the
modifier list at `niri.rs:3726-3740`, so pressing it raises the prompt and is not typed — we see the
event that changes the state. The one thing to get right is that the warning must update on
**modifier-only** keys too, which is exactly the branch that currently just calls `show_prompt`.

**Risk:** low. Layout-only, no new D-Bus, no new state source.

**Landed**, and it turned up a divergence in the neighbouring code: our key path treated Ctrl, Alt
and Super as "modifiers that raise the prompt without being typed", citing `:678-682`. That block is
the list of keys that do **not** raise it, and it holds exactly four — `Shift_L`, `Shift_R`,
`Shift_Lock`, `Caps_Lock` (`unlockDialog.js:677-682`). Everything else, Ctrl included, falls through
to `_showPrompt()`. Shift and caps are the keys you press *before* the one you meant, and setting
caps at the clock is precisely the case the warning has to survive: it is on before the entry
exists. Both fixed here.

The other trap was xkb. At the **press** of Caps Lock the event's modifier mask still describes the
state the key is about to change, so a warning driven from it appears and never leaves. Sampled
after `input()` instead, which is what the switcher next door already does for the same reason.

A review then found two bugs in it, both fixed:

- **A cached caps state is wrong for every path that is not a keystroke.** Clicking to raise the
  prompt after locking with caps already on showed no warning; clicking after a
  lock/unlock/re-lock cycle showed one that was false. GNOME reads the keymap at every sync
  (`shellEntry.js:192`), which is why it cannot have this bug — so we read xkb live too, and the
  field is only a redraw edge-detector.
- **Reversing the fade mid-flight snapped.** Deriving the ease's start from its target means a
  double-tap inside 200 ms jumps to fully opaque before fading out — a flash of a warning that was
  never up. It now eases from the current alpha.

**Resolved on the seat (2026-08-01):** the reserved row reads fine, so the divergence stands and
the growing dialog is not wanted. If that is ever revisited, it is reachable without a per-frame
re-bake — split the message into its own bake (its revision inputs are already separable) and
animate the *element* offsets, which are free.

**Toolkit-first, deferred with a reason:** GNOME hangs `CapsLockWarning` off `shellEntry`, so it is
the password *entry's* companion rather than the lock screen's. The second password surface to
land — polkit, the network agent, the keyring prompt — should lift the text, fade, cache and
gating into `widget::` beside `widget::Entry` rather than copy them.

---

## B. `Lock` replies immediately — **LANDED**

**This is smaller than the divergence note claims.** The note says we need a "first frame with the
shield up has been presented" signal. GNOME does not do that: `lock-screen-shown` is emitted from
`_lockScreenShown` (`screenShield.js:474-493`), which is the **`onComplete` of the slide-down ease**
(`:455-466`) — or called directly on the non-animated branch (`:464-465`). It is "the curtain has
landed", and we already have that state: `Curtain::Covering`.

`LockAsync` connects to the signal once, replies, and disconnects (`shellDBus.js:538-545`).

**Scope:** hold the D-Bus reply until the curtain reaches `Covering`, then answer. zbus can do this
by keeping the method future pending.

**The reply must be level-triggered, not edge-triggered.** This is the trap:

- **Locking while already covered emits nothing.** `_resetLockScreen` early-returns unless the state
  is `HIDDEN` (`screenShield.js:440-445`), so GNOME's own second `LockAsync` hangs until the D-Bus
  timeout. Our `Curtain` behaves the same way — asked to cover while covering it keeps its state and
  produces no new edge (the `(true, Showing | Covering)` arm of `set_shown`). So the rule is "reply
  when the curtain **is** down", answering immediately if it already is — never "reply when it
  *reaches* down".
- **A curtain torn down before it lands** — `SetActive(false)`, or a dismiss racing the slide — must
  resolve the pending reply rather than leak it.
- **Concurrent `Lock` calls** each need their own reply; one waiter is not enough.

Two further places GNOME's own behaviour is questionable, both worth diverging from:

- A **refused** lock never emits, so `LockAsync` never replies — `lock()` returns early on
  `disable-lock-screen` (`screenShield.js:638-641`). I would reply anyway.
- The **idle path** settles the curtain instantly (`curtain_instant`), so this must fire on that
  branch too, not only at the end of an animation.

**Risk:** low-medium. The hazard is a pending reply that outlives its caller or never fires; needs
tests for the refused lock, the already-covered lock, and the torn-down slide.

---

## H. When `ActiveChanged` fires — **LANDED, as a divergence**

**Missed by both documents until a review caught it.**

GNOME runs a *second* fade that we never ported. There are two black lightboxes
(`screenShield.js:125-137`): the long one is the ten-second idle fade we already have, and a
**short** one, `MANUAL_FADE_TIME = 300` (`:39`), which every manual lock also gets. `activate()`
always passes `fadeToBlack: true` (`:601-603`), so once the curtain lands `_lockScreenShown` waits
300 ms — the comment calls it "take a beat" — and then dims over another 300 ms (`:479-486`).

The load-bearing part is not the dimming. `_setActive(true)`, which emits `ActiveChanged`
(`:156-163`), fires from `_onShortLightbox` when that fade *completes* (`:316-319`) — so the signal
is deliberately ~600 ms late, and GNOME accepts a documented window where the screen is visibly
locked while `GetActive` answers false. Its comment (`:604-614`) says why:

> when we emit ActiveChanged(true), gnome-settings-daemon blanks the screen, and we don't want
> blank during the animation.

**Our decision (Gustavo, 2026-08-01): keep the reason, drop the mechanism.** Blanking policy
belongs to power management, not to the lock screen, and we would rather not have the two tangled
— so we do not dim on gsd's behalf and there is no beat and no second lightbox. What the lock
screen legitimately owes is that its animation is *seen* rather than replaced by an immediate
blank, and that is bought entirely by **deferring the published `active` until the curtain lands**
(250 ms, our slide) instead of until a fade we do not run.

So:

- the model's own `active` is unchanged and immediate — it is what input routing, `is_dismissible`
  and the pending-`Lock` reply all read;
- what the **session bus** sees (`GetActive` and `ActiveChanged`) waits for the curtain;
- **rises wait, falls do not**: unlocking stops claiming the screensaver is on at once, as GNOME's
  own `_setActive(false)` does (`:539`, `:581`);
- the idle path is untouched — its curtain settles instantly, so it publishes immediately, exactly
  as GNOME's non-animated branch does (`:487-490`).

This also sidesteps a refactor the faithful port would have needed. GNOME keeps `_isActive`
separate from "the shield owns the screen" (a modal grab plus `_lockScreenState`); we had collapsed
both into `active`, across ten call sites — including `settle_lock_replies`, which reads
`!is_active()` as "this lock will never land, answer the caller" and would have silently undone
item B. Deferring only what is *published* leaves every internal consumer alone.

---

## G. Hang the idle watches on our own `IdleMonitor`

Grouped up here because it is small and deletes **two** divergences at once.

GNOME uses the core idle monitor for three things:

- `add_user_active_watch` cancels the pending lock when the user comes back
  (`screenShield.js:282`, removed at `:300` and `:571`);
- `add_idle_watch(HINT_TIMEOUT * 1000)` fades the "click or press a key" hint in
  (`unlockDialog.js:395-396`);
- `add_idle_watch(IDLE_TIMEOUT * 1000)` runs `_escape` on the prompt page
  (`unlockDialog.js:666-667`).

We already *serve* an `IdleMonitor`; we just do not consume it. Today the first rides
gnome-session's `Available` presence status and the other two measure time since the last
interaction with the shield.

**Scope:** consume our own monitor for all three. Note the second and third are only equivalent to
GNOME's while the shield swallows every input — which it does — so this is about removing a
divergence rather than fixing a visible bug.

**Risk:** low, but it touches the lock timer, which is the one piece of this subsystem where a bug
locks someone out or fails to lock at all. Wants tests around the mid-fade cancel, which is where
the last bug in this area lived.

---

## C. AccountsService — **LANDED**

`RealName`, `IconFile` and `PasswordMode` all come from AccountsService now, and the picture draws:
`ImageFit::Cover` in the decode, `widget::Avatar` for the circular draw, a `bake_card_border` ring,
warmed when the account answers and again on every output add/resize (the decode is keyed on the
scale). It shares the album-art `ImageCache`, so it also had to join that cache's `retain` — its
only bound — or a track change would evict the picture.

The Other User button landed with it: `widget::IconButton` for the circular `.icon-button` shape,
gated on all four of GNOME's conditions, and `dbus::user_switching` for the action — which is not
one call but libgdm's algorithm (find our seat, reuse a live `gdm-launch-environment` greeter on it
via `ActivateSessionOnSeat`, else `CreateTransientDisplay` and only on `seat0`).

**Divergence: the gdm conversation survives the switch.** GNOME's `_otherUserClicked` cancels the
verifier (`authPrompt.js:839-852` → `:742`); it can, because it destroys and rebuilds the prompt
actor. We send `VerifierRequest::Begin` from exactly one place, driven by `ScreenShield::lock`, and
a locked screen never locks again — so cancelling would close the only channel and leave the shield
locked with nothing to authenticate against. Switching users does not end this session: it keeps
running and the user can VT back to it. Revisit once there is a re-Begin path.

**Divergence: no RTL mirroring.** GNOME flips the button to the leading edge under
`Clutter.TextDirection.RTL` (`unlockDialog.js:496-499`). Nothing else in this port is
direction-aware yet, and a lock screen that mirrors one control and not the rest is worse than a
consistently LTR one; it goes in when text direction does, as a whole.

Replaces three divergences: the user's real name and avatar come from `/etc/passwd` + GECOS today,
and `password_mode` is assumed to be "has a password".

The shell reaches AccountsService only through libaccountsservice, never raw D-Bus, so the port has
to reconstruct the protocol: bus `org.freedesktop.Accounts`, manager `/org/freedesktop/Accounts`,
per-user objects `/org/freedesktop/Accounts/User<uid>`, interface `org.freedesktop.Accounts.User`.
`FindUserByName` gets the path.

Properties actually consumed: `RealName`, `UserName`, `IconFile`, `PasswordMode`, `Locked`,
`SystemAccount`, plus the manager's `HasMultipleUsers` / `CanSwitch` for the switch-user button.

- **`PasswordMode`** is the one that matters for correctness. `screenShield.js:656-659`:
  ```js
  const lock = this._isGreeter ? true
      : user.password_mode !== AccountsService.UserPasswordMode.NONE;
  ```
  A passwordless account gets a shield that covers the screen but never locks. Our model already
  takes `password_mode_none` and is pinned by `a_passwordless_user_is_never_locked` — this is
  purely about feeding it a real value.
- **The avatar** is `IconFile`, drawn as a `background-image: cover` on a 64 px
  (`AVATAR_ICON_SIZE`, `userWidget.js:11`) bin, falling back to `avatar-default-symbolic`. The path
  is **re-checked on disk before use** (`userWidget.js:73-76`) because AccountsService will happily
  report a deleted file.
- **Loading is asynchronous and visible.** Everything is gated on `is_loaded`; the meanwhile-state
  is *blank* name labels and the default avatar (`userWidget.js:159-166`), never a placeholder
  string. Properties change at runtime and re-render (`userWidget.js:122-125`, `:200-202`).

**The fail-closed default is the whole risk.** `lock()` reads `password_mode` synchronously
(`screenShield.js:656-659`), which libaccountsservice answers from cache and a raw D-Bus port
cannot. Whenever the value is *absent* — still loading, service down, `FindUserByName` failed — it
must read as **"has a password"**, which is today's conservative assumption. Backwards, and the
first lock after boot is a shield any keypress raises. The same over time: `PasswordMode` changes at
runtime when a user sets or clears a password, so a cached `NONE` keeps every later lock unlocked.

**Trap worth naming:** the avatar is an image file whose path and contents are attacker-influenced
in the general case (any account can set its own icon). Decoding it is untrusted-content ingestion —
it needs the plain-data seam from `untrusted-content-process-seam`, even if it runs in-process for
now.

**Toolkit-first:** the avatar is GNOME's shared `UserWidget`/`Avatar`, reused by the login screen and
the user menus — 64 px (`userWidget.js:10`), circular, `background-image: cover`, symbolic fallback.
It goes in as a `widget::Avatar` with a reusable circular-cropped-texture draw, not a one-off here.

**Risk:** medium. Async property loading against a UI that is already on screen is exactly the shape
that produces "it worked when I tested it" flakes; the `is_loaded` blank state is not optional.

---

## D. Notifications on the lock screen

The biggest item, and the one a user is most likely to notice missing.

**It is not the message list.** `NotificationsBox` (`unlockDialog.js:37-355`) is a lock-screen-only
class that builds its own rows out of `St.Icon`/`St.Label`; it borrows only `MediaMessage` from
`messageList.js` for MPRIS players (`:22`, `:223`). Do not try to re-home our message list here.

**Where it sits:** a sibling of the clock/prompt stack, not a child of either
(`mainBox.add_child` order at `:657-659`), allocated at the **bottom** of the screen by
`UnlockDialogLayout.vfunc_allocate` (`:450-508`, the `height - maxNotificationsHeight` at `:473`),
with the stack's own max height reduced to leave room. Critically, `_setTransitionProgress`
(`:813-843`) never touches it — **the notification list does not participate in the crossfade.** It
stays put while the clock gives way to the prompt.

**Two row shapes**, chosen per source by `_shouldShowDetails` (`:184-187`):

```js
return source.policy.detailsInLockScreen ||
       source.narrowestPrivacyScope === MessageTray.PrivacyScope.SYSTEM;
```

- detailed (`:135-182`) — each unacknowledged notification's title and body, as markup;
- undetailed (`:98-133`) — icon, source title, and an unseen count, with the row hidden at count 0.

**Privacy filtering** is the part to get right, because getting it wrong leaks message bodies onto a
locked screen. Whether a source appears at all is `policy.showInLockScreen` (`:238`), which for an
application is master **AND** per-app `show-in-lock-screen`
(`messageTray.js:308-311`); `detailsInLockScreen` is per-app only (`:313-315`). `narrowestPrivacyScope`
is `SYSTEM` only when *every* notification in the source is system-scoped (`messageTray.js:560-563`).

**The leak vector is the count, not the policy.** Policy changes are live
(`unlockDialog.js:260-265`, `:324-345`), but `narrowestPrivacyScope` is re-evaluated from
`_countChanged` (`:297-310`), which recomputes `_shouldShowDetails` and rebuilds the row — precisely
because a USER-scoped notification arriving at a previously all-SYSTEM source has to demote it to
undetailed. Re-evaluating detailedness only when a *policy* changes renders that new notification's
body in a detailed row: exactly the leak this item exists to prevent.

**The body is app-controlled markup**, passed through `set_markup` unescaped (`:164-172`) —
untrusted-content ingestion, same rule as C's avatar.

Two smaller pieces: a row whose source holds a CRITICAL-urgency notification gets a `critical` style
class (`:189-201`), and the on-screen-keyboard `vfunc_captured_event` (`:695-700`) belongs to the
a11y milestone rather than here.

**Waking the screen:** a new notification in a visible source emits `wake-up-screen` if the master
`show-banners` is set (`:214-220`) → `ScreenShield._wakeUpScreen` (`screenShield.js:496-501`), which
only marks the user active and re-emits. It does **not** unlock or raise the prompt.

**Interaction:** none. The rows are not reactive — no activate, no dismiss. Only the MPRIS row is
clickable, and it early-returns while locked (`messageList.js:799-804`), though its transport
buttons stay live.

**Risk:** high, and the risk is a privacy leak rather than a crash. Fail-closed: a source we cannot
resolve a policy for shows nothing, not everything. Our notification subsystem is complete, so the
data is there; this is a new view over it plus the policy plumbing.

**This item owes the most tests on the list**, and is the one most likely to be under-tested, because
nothing looks broken when it leaks. The corpus wants the matrix: master off, per-app off, details
on/off, an all-SYSTEM source, and the count-driven demotion above.

**Toolkit-first:** the list scrolls (`St.ScrollView`, `:51-54`) — reuse the scrolling primitive the
message list already has rather than growing a second one.

---

## E. Fingerprint and smartcard — **fingerprint LANDED, smartcard open**

The reader half is in: `dbus::fprintd` probes `GetDefaultDevice` + `scan-type` once at startup
(gated on `enable-fingerprint-authentication`, silent on the two no-reader errors GNOME passes
over), `gdm-fingerprint` runs beside `gdm-password` on the same channel when a reader was found,
and the routing policy moved out of the async pump into a pure `route()` so it can be tested at all.

Landed with it: `MessageKind::Hint` as an ordered priority, so the reader's narration cannot talk
over the error explaining a refused password. **Divergence: no timed message queue.** GNOME shows
each message for an interval and moves on, so a suppressed hint reappears; we hold one message, so
it is dropped and returns on the reader's next `Info`.

Still open: the error wiggle (`authPrompt.js:481-493`, 3 × 65 ms, ±6 px — `animationUtils.js:87`),
and smartcard.

## E (reference notes). Fingerprint and smartcard

Additive to the gdm conversation, but not merely "start a second PAM service".

Service names are `gdm-fingerprint` and `gdm-smartcard` (`util.js:27-29`), each gated by an
`org.gnome.login-screen` key (`:33-35`).

- **Fingerprint is a real hardware probe, not a setting.** `_initFingerprintManager`
  (`util.js:343-387`) proxies fprintd (`net.reactivated.Fprint`), calls `GetDefaultDevice`, and
  reads the device's `scan-type` into `NONE`/`PRESS`/`SWIPE` (`:446-452`). Everything downstream
  gates on `serviceIsFingerprint`, which requires a *detected reader* (`:616-619`).
- **It runs in parallel with the password**, always, whenever a reader exists and it is not itself
  the foreground service (`_maybeStartFingerprintVerification`, `:714-719`).
- **Its `Info` text is discarded.** The shell replaces whatever fprintd said with its own hint at
  `HINT` priority (`:728-747`): *"(or place finger on reader)"*, or *"(or swipe finger across
  reader)"* for a swipe reader.
- **Failure accounting differs.** `_failCounter` is shared, but fingerprint counts its own
  `Problem`s (`:767`) since pam_fprintd retries without ending the conversation, and hits failure
  through a 15 ms delayed call so queued messages are not dropped (`:769-780`). **On the unlock
  screen failures are unlimited** — `_canRetry` returns true whenever `_reauthOnly`
  (`:839-842`). That is our case always, so the `allowed-failures` machinery is nearly moot for us.
- **UI difference is one thing only:** an error message from fingerprint wiggles the message label,
  3 wiggles at 65 ms (`authPrompt.js:481-493`). There is no fingerprint icon anywhere.
- **Smartcard preempts** the foreground service rather than running beside it (`util.js:490-491`),
  is monitored through gnome-settings-daemon's `org.gnome.SettingsDaemon.Smartcard` rather than
  directly (`js/misc/smartcardManager.js:6-14`), and **removing the card resets an in-progress
  prompt** (`authPrompt.js:461-479`).

**Risk:** medium, but **fully validatable on this machine** — correcting an earlier claim here
that there was no reader. The VM has an emulated USB fingerprint device our VMM relays to the
host's API (Touch ID on the Mac), `lsusb` shows `04f3:0c7d ELAN:Fingerprint`, and fprintd is
installed and D-Bus-activatable. There is a FIDO device too. So this can be built and tested end
to end rather than blind against the no-device path.

**Passkeys are a related but separate question.** GNOME 50.3 has no passkey path in `util.js` at
all — only password, fingerprint and smartcard — so that would be ours to design rather than port,
presumably as another PAM service on the same channel. Scope it with E, decide separately.

---

## F. Touch mode and the swipe gesture

Two divergences that look separate and are not: `_swipeBegin`/`_swipeUpdate`/`_swipeEnd`
(`unlockDialog.js:867-890`) drive **the same `_adjustment`** as the clock↔prompt crossfade
(`:547-555`). The swipe is not a new animation — it is a gesture-driven scrub of the transition we
already built, which is why this is worth doing after everything else rather than never.

**Part of it is testable here.** Discrete scroll flips the pages independently of the swipe tracker
— down shows the prompt, up the clock (`:557-567`) — and a synthetic scrub of the adjustment needs
no touch hardware either. Only the real gesture is unvalidatable on this machine.

The hint's wording ("Swipe up to unlock" vs "Click or press a key to unlock") needs the seat's
touch-mode property, which we do not track.

**Risk:** low to build. The scroll path and a synthetic scrub are validatable headless; only the
real touch gesture is not. Last anyway, because it is the least missed.

---

## Not planned

- **`SetActive(false)` unlocks without authenticating.** GNOME's own behaviour
  (`shellDBus.js:551-552` → `screenShield.js:515-519`), inherited deliberately. Do not "fix" it
  without changing GNOME too.
- **The clock weight is 700, not 800** — the rasterizer's ceiling, a standing divergence across the
  whole port.
- **The message block centres as a block, not per line** — invisible at one line, which is every
  message PAM actually sends.
- **Parental controls.** `Main.timeLimitsManager` reaching `LIMIT_REACHED` blocks the prompt outright
  (`unlockDialog.js:648-652`, `:928-931`; `authPrompt.js:792`). We have no time-limits subsystem for
  it to hang off.
- **The pointer is hidden until it moves** on a fresh lock screen (`screenShield.js:362-371`, called
  from `:475`). Cosmetic, and cheap whenever someone wants it.
- **A power-save-mode change slams the hint to zero** (`unlockDialog.js:440-442`) — we have no
  power-save-mode signal wired.

## Not lock screen, but found here: **no polkit authentication agent**

gnome-shell registers a `PolkitAgent.Listener` for its session (`js/ui/components/polkitAgent.js`)
and puts up the "Authentication required" dialog. We register none, so **every polkit action that
needs authentication fails immediately with `PermissionDenied` and no prompt** — there is nothing to
prompt with. Found by trying to enrol a fingerprint as the test user
(`net.reactivated.fprint.device.enroll` is `auth_self_keep`), but the blast radius is far wider:
mounting an encrypted volume, installing updates, and every Settings panel with a lock button fail
the same silent way. It is a blocker for dogfooding, agreed 2026-08-01 to be taken on after the lock
screen.

Two things make it a natural sibling of the work here rather than a separate world: the dialog is a
user widget with an avatar and a password entry, which is `widget::Avatar` plus `widget::Entry`; and
it can authenticate by fingerprint through the same machinery E just wired up.

---

## Unowned

**The "Other User" button** is in neither list, which is how a review found it. It sits beside the
clock/prompt stack (`unlockDialog.js:617-628`), is gated on `user-switch-enabled` **and** the
`disable-user-switching` lockdown (`:921-926`), goes to the login session via
`Gdm.goto_login_session_sync` while cancelling the prompt (`:901-905`), participates in the crossfade
(`:817-821`, `:838-842`) and has its own branch in the layout (`:492-508`).

It is the only reason C fetches `CanSwitch` / `HasMultipleUsers`. Decide it with C: build it there,
or write it into the divergence list as deliberately absent.
