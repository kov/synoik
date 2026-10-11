# The greeter and the login daemon

> Status: **designed; the daemon's first two steps built** (§9) — the greeter session,
> autologin, and the lock screen's password and fingerprint reauthentication. Our own replacement for GDM on the system compositor's seat: a
> small root login daemon that runs PAM, and a greeter that is synoik in a greeter session mode,
> nested in the outer like any session (`system-compositor.md`). Password and fingerprint;
> autologin; the system-only clock and quick settings GDM's greeter shows.

References: gnome-shell **50.3** for everything the greeter does and everything it calls
(`js/gdm/*.js`, `js/ui/sessionMode.js`) — that is the client being ported, so it fixes the wire —
and gdm **51.0** (`~/Projects/gdm`) for the daemon side.

---

## 0. Scope

**In:** the login daemon; the greeter UI (user grid, "Not listed?", banner, logo, auth prompt);
password and fingerprint, concurrently, as GNOME runs them; PAM conversations of any length
(expired-password changes are just more prompts — gnome-shell has no code for them,
`js/gdm/` has no `chauthtok`); reauthentication for the lock screen and for picking a user who
already has a session; autologin; the greeter's panel — clock, keyboard layout, accessibility and
system-only quick settings; user switching and logout back to the greeter.

**Out, decided:** a session chooser (one session type: synoik); smartcard (later — the client's
detection stays inert, `lock-screen-backlog.md` §E); timed login; initial setup; remote and X11
displays; per-user language selection at the greeter.

---

## 1. Shape

Three kinds of process, and only the first two are privileged:

| Process | Runs as | Does |
|---|---|---|
| `synoik-login` (daemon) | root, one per machine | owns `org.gnome.DisplayManager` on the system bus; starts the greeter session; hands out channels; spawns workers |
| `synoik-login worker` | root, one per PAM conversation | one `pam_start` … `pam_end`; for the winning login conversation, the parent of the user session until it ends |
| `synoik --greeter` | `synoik-greeter` system user, a `CLASS=greeter` logind session | the UI; a client of the daemon (auth) and of the outer (display) |

The root code is its own crate (`synoik-login`) and binary — never the compositor binary, which
stays unprivileged (system-compositor §1) and never links PAM. This is GDM's split
(`daemon/main.c:253-257` root daemon; `gdm-session-worker` per conversation,
`daemon/gdm-session.c:2367-2416`), without GDM's display management: the outer owns the display,
logind owns the seat, and the daemon only authenticates and starts sessions.

The greeter runs as one fixed system user. GDM 51 allocates dynamic greeter users per seat
(`gdm-greeter`, `gdm-greeter-2`, … from systemd's greeter UID range, served over userdb,
`daemon/gdm-dynamic-user-store.c:266-285`); that buys multi-seat, which we do not do
(system-compositor S5).

PAM is bound by a thin FFI layer of our own over `security/pam_appl.h` — the worker uses about a
dozen calls, the conversation callback's memory must be ours to zeroize, and the handle's
lifetime across the session fork must be explicit; the published crates are either an alpha
`-sys` or safe wrappers that hide exactly those.

---

## 2. The daemon

### 2.1 Wire: GDM's, the subset gnome-shell uses

The daemon speaks GDM's D-Bus interfaces under GDM's names, so the lock screen
(`src/dbus/gdm.rs`), user switching (`src/dbus/user_switching.rs`) and the greeter have **one**
client path, and stock installs that keep real GDM need no second one.

- **System bus, `org.gnome.DisplayManager.Manager`:** `OpenSession() → address` (the greeter's
  login channel) and `OpenReauthenticationChannel(user) → address`. `RegisterSession` is accepted
  and ignored.
- **Peer-to-peer channel** (a private socket, no bus; the address shape `gdm.rs:13-24` already
  speaks): `UserVerifier` (`BeginVerificationForUser`, `AnswerQuery`, `Cancel`, and the signals
  `InfoQuery`, `SecretInfoQuery`, `Info`, `Problem`, `ConversationStarted/Stopped`,
  `ServiceUnavailable`, `VerificationFailed/Complete`, `Reset`), and on login channels `Greeter`
  (`SelectUser`, `StartSessionWhenReady`, `StopConflictingSession`, signals
  `SessionOpened(service, session_id)`, `Reauthenticated`) — `daemon/gdm-session.xml:40-163`,
  `daemon/gdm-manager.xml:3-16`.
- **Refused with a named error:** `SelectSession`, `BeginAutoLogin`, `GetTimedLoginDetails`, the
  `ChoiceList` and `CustomJSON` extensions (smartcard and switchable auth; gnome-shell 50.3 uses
  neither for password or fingerprint, `js/gdm/util.js:27-29`).
- **Service names on the wire are GDM's** — the client must keep sending `gdm-password` to real
  GDM. The daemon maps `gdm-password` → PAM service `synoik-password`, `gdm-fingerprint` →
  `synoik-fingerprint`; any other name is `ServiceUnavailable`.

Owning GDM's bus name means GDM and synoik-login are alternatives, as display managers are: one
`display-manager.service`, and a bus policy (ours) letting only root own the name.

### 2.2 Who may open what

Every channel is checked twice, at open and at connect, like GDM (`daemon/gdm-manager.c:471-593`,
`daemon/gdm-session.c:2056-2083`): the caller's uid and pid from the bus (open) and `SO_PEERCRED`
(connect), then the same logind claim the outer checks for `Hello` (system-compositor §10: the
session's `UID` is the peer's, `SEAT=seat0`, and the peer's scope is that session's).

- **Login channel:** only the current greeter session's process.
- **Reauthentication channel for user U:** the greeter (U picked from its list) or U's own running
  session (its lock screen), and only while U has a session on seat0. The caller's session is the
  one its process is in, else U's display session as logind names it (`User.Display`): a
  compositor run as a user service lives in the user's manager, outside any session. GDM falls
  back the same way (`common/gdm-common.c:812-866`).
- **A channel's conversations die with its connection.** A greeter that crashes or reconnects
  starts from nothing; no PAM conversation outlives the client that drives it. (GDM sets no
  timeout at all, `gdm-session.c:1284`.)

### 2.3 Conversations: one worker per PAM service

Each `BeginVerificationForUser(service, user)` forks a worker that runs one PAM transaction. A
worker per service, not threads: `pam_fprintd` blocks inside the conversation, PAM modules keep
process state, and the losing conversation must be killable — SIGTERM, then the worker's own
cleanup runs `pam_close_session` (if it got that far), `pam_setcred(PAM_DELETE_CRED)`, `pam_end`
(`daemon/gdm-session-worker.c:1174-1198`).

The worker's sequence (`gdm-session-worker.c:1225-1808`, `2271-2397`):

1. A service with no file in the PAM search path is `ServiceUnavailable` before `pam_start`
   (`:1244-1259`). `pam_start(service, user)`; `PAM_TTY` is set only where seat0 has VTs
   (`:1354-1361`).
2. `pam_authenticate`. `PAM_USER_UNKNOWN` is reported as `PAM_AUTH_ERR`, so an unknown user and a
   wrong password look the same; `PAM_AUTHINFO_UNAVAIL`, `PAM_MODULE_UNKNOWN` and `PAM_MAXTRIES`
   reach the client as `ServiceUnavailable` (`:1392-1440`, `gdm-session.c:215-230`) — which the
   client already turns into "Fingerprint reader unavailable".
3. `pam_acct_mgmt`; on `PAM_NEW_AUTHTOK_REQD`, `pam_chauthtok(PAM_CHANGE_EXPIRED_AUTHTOK)` over the
   same conversation (`:1470-1485`).
4. `setgid`/`initgroups` (uid stays 0), then `pam_setcred(PAM_ESTABLISH_CRED)` (`:1571-1610`,
   `1700-1808`).
5. `VerificationComplete(service)`.

A prompt travels as GDM's does: the PAM conversation callback blocks; the daemon parks it and
emits `SecretInfoQuery`/`InfoQuery`; `AnswerQuery` completes it (`gdm-session.c:754-784`).
Secrets are zeroized wherever they rest — the worker, the daemon's parked answer (GDM wipes only
the worker side, `gdm-session-worker.c:900-906`; `answer_pending_query`, `gdm-session.c:769-775`,
does not) — and never cross the system bus.

### 2.4 The first to verify opens the session

GDM opens a session in **every** login conversation that reaches credentials and kills the losers
only when the greeter starts one (`gdm-session.c:338-340`, `2969-3021`); in practice the first to
open is the one `SessionOpened` names. We make that the rule and open only it:

1. The first login conversation to reach `VerificationComplete` wins; every other worker is
   SIGTERMed at once.
2. The winner puts **everything pam_systemd reads** into the PAM environment *before*
   `pam_open_session`: `XDG_SEAT=seat0`, `XDG_SESSION_TYPE=wayland`, `XDG_SESSION_CLASS=user`,
   `XDG_SESSION_DESKTOP=synoik`. (GDM pushes the desktop name only after open,
   `gdm-session.c:2873-2915` called at `:3105`, so logind never sees it — a GDM quirk we do not
   copy.)
3. `pam_open_session`; `XDG_SESSION_ID` comes back in the PAM environment, and the daemon emits
   `SessionOpened(service, id)`. The id must be real: gnome-shell looks it up in logind to find
   a conflicting session of the same user (`loginDialog.js:1267-1304`) and offers
   `StopConflictingSession`, which terminates the user's other graphical sessions
   (`gdm-session.c:1802-1822`).
4. On `StartSessionWhenReady(service, true)`, fork: `setsid`, `setuid`, `chdir(HOME)`,
   `environment.d`, stdout/stderr to the journal, `pam_end(PAM_DATA_SILENT)` in the child, exec
   `synoik-session` with `pam_getenvlist` plus `SYNOIK_SYSTEM_SOCKET`
   (`gdm-session-worker.c:2083-2236`).
5. The worker stays the session's parent; when it exits, `pam_close_session`,
   `pam_setcred(DELETE)`, `pam_end`, `killpg(SIGHUP)` (`:1862-1880`).

GDM 51 also puts `XDG_SESSION_EXTRA_DEVICE_ACCESS=render:accel` in every session's PAM
environment (`gdm-session-worker.c:1338`); what logind grants for it is checked at bring-up
before we copy it (§8 G1).

logind makes a new seat0 session `Active` on its own (system-compositor §9 S1), so the outer
fronts it with no call from anyone.

### 2.5 Reauthentication

One mechanism for both callers: a reauthentication channel runs fresh workers for U with
`PAM_REINITIALIZE_CRED` and `acct_mgmt`/`setcred` failures ignored
(`gdm-session-worker.c:1487-1492`, `1776-1783`, `2911-2915`). On `VerificationComplete` the daemon
calls logind `UnlockSession` and `ActivateSession` on U's session, then emits `Reauthenticated`
(`gdm-manager.c:947-970`).

GDM runs this inside the worker of U's own session, and uses fresh workers (its temporary channel,
`gdm-manager.c:979-1055`) only for a frozen session or one it did not start
(`gdm-manager.c:1133-1143`). We always use fresh workers: one path, and the session's worker
stays a plain parent that never talks PAM again after the session starts.

- **Lock screen:** what `src/dbus/gdm.rs` does today, unchanged.
- **Greeter, U already logged in:** gnome-shell tries the reauthentication channel first and falls
  back to a login channel on `ACCESS_DENIED` (`js/gdm/util.js:182-188`, `508-529`). Same here: a
  user with a session is switched to it, never given a second one.

### 2.6 The greeter session and autologin

At boot the daemon opens the greeter's own session: PAM service `synoik-greeter`, user
`synoik-greeter`, `XDG_SESSION_CLASS=greeter`, `XDG_SEAT=seat0`, running
`synoik --greeter` with `SYNOIK_SYSTEM_SOCKET` (GDM's `gdm-launch-environment`,
`daemon/gdm-launch-environment.c:495-510`, `568-629`). That session lives for the machine's lifetime and is
restarted if it dies. After a login it stays, in the background, ready: logout and "switch user"
activate it (§5), so there is never a greeter to spawn on the interactive path.

**Autologin:** if configured (`[daemon] AutomaticLoginEnable/AutomaticLogin` in
`/etc/synoik/login.conf`, GDM's keys), at boot the daemon runs a `synoik-autologin` conversation
(no prompts) for that user and opens the session as in §2.4, then starts the greeter behind it.
Once per boot: after that user logs out, the greeter is what comes up.

---

## 3. PAM stacks

Shipped as `synoik-*`, modelled on GDM's Fedora and Arch stacks (`data/pam-redhat/*.pam`,
`data/pam-arch/*.pam`), so we do not depend on the gdm package:

- **`synoik-password`:** the distro password substack (`password-auth` / `system-local-login`),
  `pam_gnome_keyring` (auth; password `use_authtok`; session `auto_start`), the distro session
  stack (which carries `pam_systemd`).
- **`synoik-fingerprint`:** `pam_fprintd` (the distro `fingerprint-auth` substack where there is
  one), password changes denied, the same session stack.
- **`synoik-greeter`:** `pam_succeed_if user = synoik-greeter` for auth and account, session with
  `pam_systemd`; no password.
- **`synoik-autologin`:** `pam_permit` auth, the normal account and session stacks.

Each distro substack is wrapped as GDM 51 wraps its own — `include <substack>` then `pam_deny`
(`data/pam-redhat/gdm-service-auth-substack.pam.in`, `data/meson.build:103-120`) — so an empty or
missing distro file denies instead of authenticating anyone. Where the distro stacks carry
`pam_oo7` beside `pam_gnome_keyring`, ours do too.

Rate limiting is `pam_faillock` in the stack; the greeter's `allowed-failures` (§4.3) only decides
when the prompt resets.

**The keyring after a fingerprint login.** There is no password to unlock it with, so
`synoik-fingerprint` does what GDM's does: a module ahead of `pam_gnome_keyring` hands it the disk
encryption passphrase from the kernel keyring's `cryptsetup` key as `PAM_AUTHTOK`, zeroized after
(`pam_gdm/pam_gdm.c:48-99`), and is skipped (`[success=ok default=1]`) when there is none. Ours is
`pam_synoik`, since `pam_gdm` ships with the gdm package. Not in this version: the user's
password sealed in the TPM and unsealed for this, so the keyring opens without disk encryption
too — to be designed on its own.

---

## 4. The greeter

`synoik --greeter`: the same compositor, nested (`backend::nested`), in a **greeter session
mode** — a single model carrying gnome-shell's `restrictive` + `gdm` flags
(`js/ui/sessionMode.js:22-65`): `is_greeter`, `allow_settings = false`,
`has_windows = has_overview = has_workspaces = false`, `show_calendar_events = false`, dark colour
scheme, components `networkAgent` (always asking, `components/networkAgent.js:679`) and
`polkitAgent`. Today's lock gating is two ad-hoc booleans (`Synoik::is_locked`,
`screen_is_covered`); the mode replaces them as the place a surface asks what it may show.

### 4.1 Panel: clock and system-only quick settings

`login-screen` panel style — transparent, sliding in from the top at startup
(`js/ui/layout.js:797-809`). Left empty; centre the date menu; right keyboard layout and quick
settings (`sessionMode.js:59-63`).

- **Clock:** the date menu with a plain month calendar — no events, no world clocks or weather, no
  message list (`js/ui/dateMenu.js:999-1015`).
- **Keyboard layout:** the system's layouts from localed, not a user's GSettings
  (`status/keyboard.js:361`); no "Show Keyboard Layout", no settings entry.
- **Quick settings:** everything hardware decides stays (network, Bluetooth, volume, brightness,
  power profiles, night light, dark style, Do Not Disturb, rfkill and the privacy indicators).
  Gone: screenshot (`status/system.js:117`), settings button (`:161-163`, `allowSettings`), lock
  (`systemActions.js:353`), log out and switch user (`:431`, `:446`), Thunderbolt
  (`status/thunderbolt.js:280`), every "Settings" menu entry (`popupMenu.js:689-691`, `723`).
  The battery pill shows but does not open settings (`status/system.js:66-68`).
- **Power menu:** Suspend, Restart, Power Off — all hidden by `disable-restart-buttons`
  (`systemActions.js:375-418`). gnome-shell routes them through gnome-session, which the greeter
  does not run; we call logind directly (`Suspend`, `Reboot`, `PowerOff`), allowed for the active
  greeter session by logind's default polkit policy.
- **Accessibility:** not in the panel in this mode; it is the dialog's bottom-right button
  (§4.2).

### 4.2 The login dialog

- **Background:** flat dark, no wallpaper (`layout.js:552`; `SystemBackground` `#282828`,
  `background.js:109`).
- **Users:** a grid of avatar tiles — the design-dreams layout, not GDM's list
  (`dreams-assessment.md` §9). The users are AccountsService's, skipping system accounts, locked
  accounts and users with no name, live on `UserAdded`/`UserDeleted`/`Changed`
  (`js/gdm/loginDialog.js:262-275`, `1530-1545`); a user with a running session is marked
  (`:110-115`). An empty list, or `disable-user-list`, goes straight to the username prompt
  (`:914-944`).
- **Not listed?** under the grid: asks "Username", then begins verification for what was typed
  (`:1141-1165`).
- **Banner** (`banner-message-enable`, `-source`, `-text`, `-path`) top-centre, moving into the
  free space above the prompt (`:946-1010`, `807-867`); **logo** (`logo`) bottom-centre
  (`:1018-1039`); **accessibility** button bottom-right, a two-column menu of the ten
  accessibility toggles (`:415-459`).

### 4.3 The auth prompt

`src/unlock_dialog.rs` is already a bus-free conversation machine — entry, masking, peek, the
message queue with read-time floors, the wiggle, `submit`/`cancel` — and `src/dbus/gdm.rs`'s
`route()` already runs `gdm-password` in front with `gdm-fingerprint` beside it, rewriting the
reader's text into the hint. The greeter reuses both; what it adds is what differs between
gnome-shell's two modes (`authPrompt.js:162-168`, `197`, `264-267`):

- the user is chosen (or typed), not fixed;
- a **Cancel** button (back to the grid), hidden while asking for a username with the list
  disabled;
- after `allowed-failures` (default 3, `js/gdm/util.js:165-167`) failed attempts the prompt resets
  to the grid instead of retrying forever as the lock screen does (`:839-842`);
- the channel is the login one (`OpenSession`), falling back from reauthentication (§2.5).

Honoured `org.gnome.login-screen` keys: `enable-password-authentication`,
`enable-fingerprint-authentication`, `allowed-failures`, `disable-user-list`, `logo`,
`banner-message-*`, `disable-restart-buttons`. Not honoured: `enable-smartcard-authentication`
(out of scope). The greeter reads them from a system dconf profile, as GDM's `DCONF_PROFILE=gdm`
does.

### 4.4 Leaving the greeter

gnome-shell fades its own UI out over 250 ms before starting the session
(`loginDialog.js:1254-1265`). We do not: the new session becomes `Active`, and the outer's switch
crossfade (system-compositor §5) is the transition. A user session's first frame takes seconds,
not the 300 ms the outer waits before fading to the plate — but the outgoing picture here is the
greeter, which shows nothing private, so the outer **holds a greeter-class outgoing picture until
the incoming frame** (bounded at 10 s) rather than fading to the plate. The 300 ms rule stays for
user → user switches, where it exists to take the previous user's screen away.

---

## 5. Switching and logout

- **Switch user** (lock screen button, user menu): activate the greeter session. Today
  `user_switching::find_greeter` matches `Service == gdm-launch-environment` and falls back to
  GDM's `CreateTransientDisplay`; it also matches `synoik-greeter`, and there is no fallback to
  need — the greeter always exists (§2.6).
- **Coming back:** when logind makes the greeter `Active` again it resets to the grid
  (`loginDialog.js:1177-1202`).
- **Logout:** the session's worker sees it exit and closes PAM (§2.4); logind fronts whatever it
  activates next, and the daemon activates the greeter if nothing else is.
- **Chord:** the greeter is seat0's earliest session, so Ctrl+Alt+F1 is the greeter, as VT1 is
  under GDM; users follow in login order (system-compositor §10 Chord).

---

## 6. Security rules

- No PAM and no secret in the compositor process — the greeter and the lock screen only relay
  answers, as today (`gdm.rs` module doc).
- Root code is small, separate, and single-purpose: no rendering, no toolkit, no config beyond
  autologin.
- Secrets: private sockets only, zeroized at rest, never logged (the client's `Answer` already
  redacts in `Debug`).
- Every channel is bound to a logind session claim (§2.2) and dies with its connection.
- An unknown user is indistinguishable from a wrong password.
- The greeter user owns nothing but its own session: an `Active` greeter-class session strips every
  user's device ACLs and grants none (system-compositor §9 S1), which is what we want here.

---

## 7. Bring-up order

1. `synoik-login` with the greeter session and autologin only: replaces the harness's
   `synoik-session@` units; boots to an autologged session and a background greeter.
2. Channels and workers: password, then fingerprint beside it (the VM's emulated reader relays to
   Touch ID, `lock-screen-backlog.md` §E); the lock screen against our daemon.
3. `synoik --greeter`: session mode, panel, grid, prompt.
4. Switching, logout, the greeter-hold rule in the outer.

---

## 8. Open questions

**G1 — `XDG_SESSION_EXTRA_DEVICE_ACCESS`.** GDM 51 asks logind for `render:accel` on every
session; read what logind does with it before copying it.

**S4 (system-compositor) stays open:** the lock screen remains in the inner; this design does not
need it moved.

---

## 9. What is built

§7 step 1: `synoik-login` (crate `synoik-login`, `resources/synoik-login.service`,
`resources/pam/`, the greeter user's `sysusers`/`tmpfiles`) opens the greeter's session and the
autologin session, and nothing else — no channels, no D-Bus name, no prompts.

- **Daemon** (`daemon.rs`): at start, the greeter's session (`synoik-greeter`, `CLASS=greeter`,
  running `synoik --greeter`, which is `--session` until §4 exists), then autologin once per boot
  (`/etc/synoik/login.conf`, a marker in `/run/synoik-login`). It restarts a greeter that dies
  and, since a new session takes the seat, gives the seat back to whatever was active before it
  reopened one. It follows seat0 through logind's `PropertiesChanged` and activates the greeter
  when seat0 has no active session. Stopping it ends the greeter and leaves users' sessions.
- **Worker** (`worker.rs`, `pam.rs`): §2.3-§2.4 without a client — the conversation refuses any
  prompt. Reports the logind session id on a pipe; forwards SIGTERM/SIGHUP to the session and
  kills it 5 s after a SIGTERM it ignored.
- **SELinux** (Fedora, enforcing): the daemon must run in GDM's domain, or `pam_selinux`'s switch to
  the user's context is denied — `synoik-login` is labelled `xdm_exec_t`. The greeter then runs
  in `xdm_t`, so what it writes needs GDM's types: the outer's `/run/synoik-system` is
  `xdm_var_run_t`, the greeter's home `xdm_var_lib_t`. `xdm_t` may read `/run/systemd/seats`
  but not watch it, hence the D-Bus subscription. These are `semanage fcontext` rules today; a
  package carries them.

Measured 2026-10-11 on the enhanced F44 guest under Limina with the `CONFIG_VT=n` kernel: boot opens
the greeter (`c1`) then `claude`'s session (`2`, active), both attached to the outer;
Ctrl+Alt+F1/F2 switch between them. Restarting the daemon replaces the greeter and gives the seat
back to session 2 at once. Logging out (`loginctl terminate-session 2`) closes its PAM session in
its worker, and logind leaves seat0 with **no** active session — the greeter is in front 15 ms
later because the daemon activates it. Left: the greeter's user manager may not start IBus
(`xdm_t` denied `start`).

§7 step 2: reauthentication, for the lock screen.

- **Bus** (`bus.rs`): owns `org.gnome.DisplayManager` (policy `resources/synoik-login.dbus.conf`)
  and serves `Manager.OpenReauthenticationChannel` with the §2.2 checks; `RegisterSession` and
  `RegisterDisplay` are accepted and ignored.
- **Channel** (`channel.rs`): an abstract socket with a random name — no file to label, nothing
  reachable but by the address — for the one uid that asked; `UserVerifier` on it, one
  `synoik-login verify` worker per service, GDM 51's signals in GDM 51's order. On
  `VerificationComplete` the daemon unlocks the session through logind. The client hanging up ends
  every conversation.
- **Worker** (`verify.rs`, `ipc.rs`): the PAM conversation relayed over a socket pair as framed
  messages; answers are wiped where they rest, and cut at `PAM_MAX_RESP_SIZE`. Refusals carry
  GDM's wording.
- **Stacks:** `synoik-password` and `synoik-fingerprint` after GDM 51's Fedora ones, each distro
  substack wrapped with `pam_deny`. `pam_synoik` (§3) waits for the greeter: a lock screen's
  keyring is already open.

Measured 2026-10-11 on the same guest: the lock screen (`src/dbus/gdm.rs`, unchanged) locks against
the daemon; a wrong password shows "Sorry, password authentication didn't work. Please try again."
and the client re-begins; the right one unlocks. Through a GDBus client on the channel:
`gdm-fingerprint` with the guest's stack (no reader) gives `ServiceUnavailable('')`,
`VerificationFailed`, `ConversationStopped` — the sequence the lock screen turns into "Fingerprint
reader unavailable" — and `gdm-smartcard` the same, with no worker. With Limina's Touch ID reader
(`authselect enable-feature with-fingerprint`, a print enrolled with `fprintd-enroll`), the lock
screen runs `gdm-fingerprint` beside `gdm-password` and a touch unlocks the session; a reader
another client holds (Settings' fingerprint dialog keeps its claim while open), or a scan that
times out (pam_fprintd's 30 s), reports unavailable and leaves the password working.
