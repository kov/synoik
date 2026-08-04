# Session end: letting applications exit cleanly

How logout / power off / restart tears the session down, why apps used to die badly doing it, and
what the compositor now does about it.

Companion to the diagnosis in `overview-port.md` §4, which is where the bug was first written up.

---

## 1. What GNOME actually does

Worth stating plainly, because the intuitive answer is wrong in three places.

**The compositor asks its clients nothing.** mutter's SIGTERM handler
(`gnome-shell/src/main.c:551-579`) calls `meta_context_terminate`, which is a bare
`g_main_loop_quit` (`mutter/src/core/meta-context.c:519-530`). On the way out
`meta_wayland_compositor_prepare_shutdown` calls `wl_display_destroy_clients`
(`mutter/src/wayland/meta-wayland.c:822`) — that closes the sockets, it does not ask anyone to
leave. `xdg_toplevel.close` is sent from exactly one place
(`meta-wayland-xdg-shell.c:1167`, reachable only from `meta_window_delete`, i.e. a user closing a
window), and nothing sweeps the window list at session end.

**gnome-session's EndSession phases reach nobody.** `gnome-session-service` still runs the full GSM
phase machine — `QUERY_END_SESSION`, `END_SESSION`, `QueryEndSession`/`EndSession`/
`EndSessionResponse` over `org.gnome.SessionManager.ClientPrivate` — but only for clients that
called `RegisterClient`. GTK3 did (`gtkapplication-dbus.c`); **GTK4 does not**: the installed
`libgtk-4.so.1` contains no `RegisterClient`, `ClientPrivate`, `QueryEndSession` or
`EndSessionResponse` at all. gnome-shell has never registered either. In a GNOME 50 session those
phases are a formality over an empty client set.

**systemd is what stops the apps.** Apps launched by the shell go into a transient scope
`app-gnome-<id>-<pid>.scope` (libgnome-desktop's `gnome_start_systemd_scope`, called from
`shell-global.c:1181-1207`). The scope itself is created with only `Description`, `PIDs` and
`CollectMode` — the property that matters comes from a drop-in gnome-session ships for the
unit-name *prefix*:

```
/usr/lib/systemd/user/app-gnome-.scope.d/override.conf   (gnome-session-50.1)
[Unit]
CollectMode=inactive-or-failed
PartOf=graphical-session.target
[Scope]
TimeoutStopSec=5s
```

`gnome-session-shutdown.target` has `Conflicts=graphical-session.target`, so ending the session
queues a stop job for the target, and `PartOf` drags every app scope into it. That SIGTERM is the
entire "apps exit cleanly" mechanism in GNOME. (For a GTK4 app with no SIGTERM handler, "cleanly"
means the default disposition — no state save. It is merely not a crash.)

### The race

The app units and `org.gnome.Shell@user.service` land in the **same stop transaction, and the
ordering that exists between them is not the one it looks like**. Worth being precise, because the
shape of the race decides what a fix may touch.

There *is* ordering, and it is in our favour. `gnome-session-shutdown.target` carries
`After=graphical-session.target` alongside the `Conflicts=`, so the target's stop job completes
before the shutdown target starts; and the shell is `Before=gnome-session-initialized.target` on
start, which reverses to "the target stops, then the shell". Both traces below show the shell being
asked last, after `graphical-session.target` has reported `Stopped`.

**What is missing is that a target's stop job completing does not mean its members have finished
stopping.** `PartOf=` propagates a stop *job* to each member; nothing orders that job before the
target's own. So the target can report `Stopped`, the shell can be SIGTERMed and unwind, while an
app is still inside its five seconds. That is the whole race: the compositor is ordered after a
line that does not mean what it says.

Measured on our own session (journal, 2026-08-03 14:10:15; `systemd[57776]` is the gsrs user
manager, and that Epiphany was launched by us):

```
14:10:15.837  Stopping app-gnome-org.gnome.Epiphany.desktop-190833.scope...   SIGTERM to the app
14:10:16.175  Stopped target graphical-session.target                         ← the app is still up
14:10:16.179  Stopping org.gnome.Shell@user.service...                        SIGTERM to us, +341 ms
14:10:16.441  epiphany[190833]: Lost connection to Wayland compositor.        we went first
14:10:16.457  Stopped org.gnome.Shell@user.service.
14:10:16.741  Stopped app-gnome-…-190833.scope                                app done, +562 ms after
```

**Read the second and last lines together.** `graphical-session.target` reported `Stopped` 566 ms
before the scope it consists of actually stopped, which is the propagation gap above, in one
session.

**Read the fourth line.** The app got a 341 ms head start and needed 903 ms; our unwind took 262 ms.
So the compositor went away *first*, and Epiphany spent its last 300 ms shutting down against a dead
socket. This trace is not a near miss that luck saved — it is the failure, captured. It merely did
not look like one, because GTK4 prints `Lost connection to Wayland compositor.` and exits, where
GTK3's `gdk/wayland/gdkeventsource.c` raises the same EPIPE as a fatal `g_error` and Firefox turns
it into the `ANOM_ABEND` recorded in `overview-port.md`. Same race, same loss, different toolkit
manners.

Which also means the head start is not the safety margin it looks like. 341 ms only helps an app
that finishes inside it; anything slower is relying on the compositor's own unwind being slower
still, and ours is fast.

---

## 2. What we do

**Since 2026-08-03: the same thing, and nothing more.** SIGTERM stops the compositor, the `Quit`
action stops the compositor, confirming the end-session dialog answers gnome-session at once. We do
not wait for clients, do not stop any unit, and do not tell systemd we are stopping. §6 is the
argument for that; this section is what is left after it.

A drain used to sit on every one of those paths — the compositor stayed in the event loop, serving
Wayland, until the last client window was gone or five seconds passed, and asked the apps to quit
itself because nothing else had. It is gone. What survives is the part that was never about waiting:
making sure the apps GNOME's teardown is *supposed* to reach can actually be reached.

### 2.1 Scopes carry their session membership explicitly

`start_transient_scope` (`src/utils/spawning.rs`) sets `PartOf=graphical-session.target`,
`Description` and `TimeoutStopUSec` itself, on both scope prefixes.

Our `app-gnome-*` scopes already inherited all of that from gnome-session's drop-in — the prefix was
matched on purpose, and `DropInPaths` on a live scope confirms it. Setting it directly does two
things that inheritance does not: it covers `app-niri-*` (the `spawn` path, which matches no drop-in
and got nothing), and it stops the property that makes logout work from resting on a file shipped by
the package this fork intends to replace.

**This is now the only lever we have on app teardown**, and it is the right one: it is a
*declaration*, resolved by systemd along with everyone else's, rather than an action of ours. An app
of ours that is missing it is an app the teardown will not stop.

> Reading these properties back has a trap. `systemctl show` on a unit that no longer exists answers
> with **stub defaults** rather than an error — `PartOf=`, `TimeoutStopUSec=45s`,
> `Description=<the unit name>` — which reads exactly like "the drop-in never applied". Check
> `Transient=yes` / `DropInPaths=` before believing a negative result.

### 2.2 An app must not inherit the compositor's signal mask

The oldest real bug here, and the one that made every other theory look plausible: **every app
launched from the dash, grid or search started life unable to receive SIGTERM.**

`signals::block_early` (`src/main.rs:63`) blocks SIGHUP/SIGINT/SIGTERM process-wide, so the calloop
`Signals` source can own them — and a blocked mask survives both `fork` and `execve`. GIO's
`g_app_info_launch` forks out of *our* process and offers no hook between the two, so the app came
up deaf to the one signal that asks it to quit. Nothing but SIGKILL could touch it. The
`spawn`/`spawn-at-startup` path had always known this and cleared the mask in `pre_exec`
(`utils::spawning:157`), as does xwayland-satellite; the app path was the one that did not.

Measured, three logouts in a row: OBS, Firefox and Epiphany each sat through their five seconds in
total silence and were killed — OBS then reporting "Crash or unclean shutdown detected" on the next
start. The control that named it: the same OBS flatpak on a GNOME 50 session, same machine, reacted
to the same `StopUnit` in **0.8 ms** and was done in 630 ms. Reproduced away from the compositor,
and the A/B is one line:

```
plain launch()             SigBlk=0000000000004003   state after SIGTERM = S   (alive)
as_manager + child setup   SigBlk=0000000000000000   state after SIGTERM = Z   (gone)
```

**Note the divergence underneath it: GNOME never has this problem, because it never blocks.**
mutter and gnome-shell take SIGTERM with `g_unix_signal_add` (`mutter/src/core/mutter.c:121`,
`gnome-shell/src/main.c:578-579`), which installs an ordinary `sigaction` handler writing to GLib's
wakeup pipe — no `sigprocmask` anywhere, so a child forks with a clean mask and there is nothing to
undo. We block because calloop's `Signals` source is signalfd, and signalfd only delivers a signal
that is blocked. The mechanism is ours to keep; the *observable* behaviour has to be GNOME's, which
means every fork path must clear the mask, and that is what the fixes below do.

So `launch_default` (`src/app_system.rs`) uses `launch_uris_as_manager_with_fds`, whose `user_setup`
runs in the child between fork and exec — the same window `pre_exec` uses. Three things about that
choice are load-bearing:

- **`DBusActivatable` apps keep plain `launch()`.** They are forked by the bus, not by us, so they
  never had our mask; and `as_manager` never activates, it always spawns, which would lose the
  activation entirely.
- **`pid_callback` is deliberately unused.** `as_manager` still emits `launched` on the context, so
  `scoped_launch_context` stays the single place a scope is started rather than becoming one of two.
- **`DO_NOT_REAP_CHILD` must stay off**, which is the opposite of what it looks like. Setting it
  makes the app our direct child — and nothing here reaps one, because the child watch GIO hangs on
  the thread-default `GMainContext` is never iterated by a compositor running calloop, so every app
  the user launched would leave a zombie on quit. It went in on the assumption that the flag was
  needed for a usable pid; measured, it is not. Without it glib spawns through an intermediate fork,
  the app is reparented to init and reaped there, and the `launched` signal still reports the
  **app's** pid rather than the intermediate's, so the scope still gets a live process:

  ```
  as_manager + DO_NOT_REAP_CHILD   pid=…  comm='sleep'  our child: yes   after exit: Z
  as_manager, no DO_NOT_REAP       pid=…  comm='sleep'  our child: no    after exit: reaped
  ```

  The test asserts the launched pid is *not* our child, so this cannot quietly come back.

**Desktop actions come in by a different door**, and were left deaf for one commit longer.
`g_desktop_app_info_launch_action` has no `as_manager` variant either, so `launch_action` rebuilds
the action as a standalone `DesktopAppInfo` — `Exec` and `Name` from the `Desktop Action` group,
`Path`/`Terminal`/`StartupNotify`/`StartupWMClass` carried over from the parent because they say how
to *run* it and an action has no opinion — and sends that back through `launch_default`, so there
stays exactly one place that knows how to fork safely. `DBusActivatable` apps are exempt here too:
their actions go out as `ActivateAction` and fork nothing. The scope keeps the *parent's* app id,
which is why `scoped_launch_context` now takes one rather than reading it off whatever GIO hands
back — a synthesized entry has no id of its own and would otherwise be scoped under its executable.

`a_launched_app_can_be_asked_to_quit` pins both doors, by reading `SigBlk` out of `/proc` for a child
of the real `launch_default` and the real `launch_action`. It blocks the mask on its own thread
rather than the process, so a parallel test binary is unharmed and the fork still inherits from the
forking thread — which is exactly the compositor's situation. Both arms confirmed to fail against
the calls they replaced. The action arm also asserts `list_actions().len() == 1` before proving
anything: a `Desktop Action` group is invisible without an `Actions=` key in the main group, and
without that guard the arm would have passed by testing nothing.

**Measured on the seat afterwards**, the same OBS that had been SIGKILLed three logouts running
reacted to the teardown's SIGTERM in 0.96 ms, against GNOME's 0.8 ms, and reported no unclean
shutdown on the next start. That number is the one that matters now: with no drain, an app's whole
budget is its own scope's `TimeoutStopSec`, and being able to hear the signal is all we can give it.

### 2.3 No `xdg_toplevel.close` sweep

Deliberate, and unchanged. mutter has none — `meta-wayland-xdg-shell.c:1167` is reachable only from
`meta_window_delete`, i.e. a user closing a window — and sending one at session end would put
"save changes?" dialogs in front of a logout the user has already confirmed. GNOME's answer to
unsaved work is the inhibitor protocol; see §3.

## 3. Known gaps

Four of the gaps that used to live here — terminal-spawned clients, D-Bus-activated apps being
unreachable, a `Quit` under a non-systemd init, a withdrawal arriving during a confirm drain, and a
stuck app costing two five-second budgets — were all gaps *in the drain*. They went with it. What is
left is the same set GNOME has.

- **An app that ignores SIGTERM is SIGKILLed after its scope's `TimeoutStopSec`**, and loses the
  socket whenever we happen to exit, which may be sooner. This is GNOME's behaviour and its exposure;
  §1's race is the shape of it. §2.2 is the only thing that changes the odds, by making sure the
  signal arrives at all.
- **Inhibitors are still parsed and discarded**, and this is the one gap here that is a missing
  *feature* rather than a limit. `EndSessionDialog.Open` takes a list of inhibitor object paths
  (`src/dbus/gnome_session.rs:59`) and the dialog does not show them, so an app with unsaved work
  has no way to say so — which matters doubly for us, because §2.3 deliberately declines the
  `xdg_toplevel.close` sweep on the grounds that inhibitors are GNOME's designed answer to exactly
  that problem. Declining the sweep and never building the answer leaves the user with neither.

  **Owned by the end-session dialog audit** (`docs/fork/end-session-dialog-port.md`, to be written),
  not by this document: the filtering rules live in the dialog. GNOME loads each path as a
  `GnomeSession.Inhibitor`, resolves it to an app (`findAppFromInhibitor`,
  `js/ui/endSessionDialog.js:153`), and lists only those whose flags include
  `InhibitFlags.LOGOUT` *and* which resolve to a real app — services and non-logout inhibitors are
  dropped (`_onInhibitorLoaded`, `:571-591`). Dead inhibitors are expected and tolerated (`:158`).
- **No `RegisterClient` participation.** We do not register as a session client, matching
  gnome-shell. Since GTK4 dropped registration this protocol is dead for apps too, so there is
  nothing to gain until something starts speaking it again — the new `xx_session_v1` toplevel
  session protocol is about *restoring* windows, not quitting them.

---

## 4. D-Bus-activated apps: GNOME restarts the bus

A `DBusActivatable=true` app is launched by the message bus, not by us, so the shell never sees a
pid to put in a scope — `shell-global.c:1194-1196` bails out with "it's already in its own unit",
and `app_system.rs` does the same. dbus-broker puts it in a transient
`dbus-:1.x-org.foo@N.service`, and on this machine that unit's only drop-in is uresourced's
`dbus-.service.d/00-uresourced.conf`, which sets `Slice=app.slice` and nothing else. Checked live:
`PartOf=`, `BindsTo=`, `Requisite=` are all empty. The `graphical-session.target` stop does not
touch these units at all.

What actually kills them is at the bottom of `gnome-session-shutdown.target`, and GNOME says what it
is in the file:

```
# We trigger a restart of DBus after reaching the shutdown target this
# is a workaround so that DBus services that do not connect to the
# display server are shut down after log-out.
# This should be removed when the relevant services add a
# PartOf=graphical-session.target
# Historic bug: https://bugzilla.gnome.org/show_bug.cgi?id=764029
Wants=gnome-session-restart-dbus.service
Before=gnome-session-restart-dbus.service
```

`gnome-session-ctl --restart-dbus` bounces `dbus-broker.service`; every activated service loses its
bus connection and exits. A self-described workaround, aimed at *services*, that takes GUI apps with
it as a side effect.

**Its timing is bad for exactly the case it covers.** From the same logout:

```
14:10:16.179  Stopping org.gnome.Shell@user.service...      compositor SIGTERM
14:10:16.604  Stopping dbus-broker.service...               bus restart, +425 ms
```

The bus restart lands **after** the compositor has been told to stop. So under stock GNOME a
D-Bus-activated app is *more* exposed than a scoped one, not less: a scoped app at least gets a
341 ms head start, while this one is not asked to leave until 425 ms after the shell was.

**The drain could never have covered it, which is worth keeping now that the drain is gone.** An
early revision of this section argued that it did — the drain keeps us serving Wayland for 5 s, the
bus restart lands ~425 ms after we are told to stop, so the app gets to close its own socket. That
reasoning had a hole in it: the restart is not at a fixed offset from our *SIGTERM*, it is downstream
of our *exit*. `gnome-session-restart-dbus.service` is pulled in by `gnome-session-shutdown.target`,
which `org.gnome.Shell@user.service` triggers through `OnSuccess=`. Every second the drain waited
pushed the bus restart back by the same second, so it could never arrive during the drain — the
deadline moved with us.

Measured 2026-08-03, back when the drain existed, on a logout with a keyring password prompt open
(`gcr-prompter`, in `dbus-:1.2-org.gnome.keyring.SystemPrompter@0.service`):

```
16:26:51.495  drain 2 starts (after drain 1 timed out on it)
16:26:56.501  clients did not exit within 5s: gcr-prompter
16:26:56.598  gcr-prompter: Error reading events from display: Broken pipe   ← the yank
16:26:56.641  Stopped org.gnome.Shell@user.service
16:26:56.645  Started gnome-session-restart-dbus.service                      ← 47 ms too late
```

Ten seconds of waiting, both budgets spent, and the client still went out on `Broken pipe`. **A
mechanism whose own deadline moves with the wait is one waiting cannot fix** — this was the clearest
single instance of that, and it generalises to the drain as a whole (§6). Today we exit at once, the
bus restart follows, and this client class is exposed exactly as it is under GNOME.

Do not read "the app has no scope" as "the app is unprotected"; read it as "this one is the bus's to
end, not ours", which is the conclusion this section exists to record.

## 5. Starting the session action: D-Bus, not `gnome-session-quit`

The quick-settings **Restart… / Power Off… / Log Out…** rows call `org.gnome.SessionManager`
directly (`PopoverAction::SessionRequest` → `Niri::request_session_action` →
`dbus::gnome_session::request_session_action`). That is what gnome-shell does:
`this._session.LogoutAsync(0)`, `ShutdownAsync(0)`, `RebootAsync()`
(`js/misc/systemActions.js:483-501`).

They used to spawn `gnome-session-quit --logout` / `--reboot` / `--power-off`. The helper does the
same D-Bus call, but from a fresh GTK process, and that start was on the logout path: measured
across ten logouts on the seat (2026-08-03), 0.69–1.54 s between the scope starting and the session
beginning to end. The keyboard actions (`Action::Logout` and friends) had always used the direct
call; only the menu rows went the long way round.

**Suspend goes the same way**, and that is gnome-shell's choice rather than an obvious one:
`activateSuspend` is `this._session.SuspendAsync()` on the *same* proxy (`:509`), not a call to
logind, even though gnome-session only forwards it there. It used to spawn `systemctl suspend`.
Unlike the other three it ends nothing and opens no dialog, so `SessionRequest::Suspend` is the one
variant that never comes back to us as `EndSessionDialog.Open`.

**One asymmetry is gnome-shell's and is pinned by
`quick_settings_system_rows_call_gnome_session_directly`:** logout hides the overview first
(`Main.overview.hide()`, `:487`), power-off, restart and suspend do not.

---

## 6. Why the race is lost — and why the drain went away

Written 2026-08-03, after a logout with gnome-terminal open cost the full five seconds. **Acted on
the same day: the drain is gone**, and §2 is what the compositor does now. Kept because the reasoning
is the reason, and because every part of it was wrong once.

### 6.1 There *is* ordering, and it is on our side

The earlier reading of §1 — "same transaction, no `After=` between them" — is not quite right, and
the correction matters. `gnome-session-shutdown.target` carries `After=` alongside `Conflicts=` for
both `graphical-session.target` and `gnome-session-initialized.target`, and
`org.gnome.Shell@.service` is `Before=gnome-session-initialized.target` on start, which reverses to
"that target stops, then the shell". Both traces in §1 show it holding: the shell is asked to stop
*after* `graphical-session.target` has reported `Stopped`.

**What that ordering does not cover is the members.** `PartOf=` propagates a stop *job*; nothing
orders that job before the target's own. So the target reports `Stopped` while its units are still
inside their `TimeoutStopSec=5s`, and the compositor's "we go last" guarantee is against a line that
does not mean what it says. In the 14:10 trace `graphical-session.target` reported `Stopped` at
`16.175` and the Epiphany scope stopped at `16.741` — 566 ms of app life on the far side of the
guarantee, with the shell SIGTERMed at `16.179` and gone by `16.457`.

That is the race, and it is why it is lost: **the ordering exists on the targets, and the processes
are in the leaves.**

### 6.2 The gnome-terminal five seconds is not that race at all

It is ours, and it is a deadlock we built. Journal, 2026-08-03 22:41, one gnome-terminal up:

```
22:41:24.432  session ending, waiting for clients to exit        ← the *confirm* drain starts
22:41:29.398  clients did not exit within 5s…: org.gnome.Terminal
22:41:29.440  Stopping gnome-terminal-server.service...          ← first time anyone asks it
22:41:29.457  Stopped gnome-terminal-server.service              ← 17 ms
```

The client needed 17 ms and we waited 5 s, because for the whole of those 5 s **nobody had asked
it**. The confirm drain held the `Confirmed*` answer until the apps were gone; gnome-session starts
the teardown when we answer; the teardown is what stops the apps. We waited for an event that our
waiting was what prevented. `stop_app_scopes` existed to paper over exactly that — the confirm
drain's substitute for the teardown it was holding up — and it missed
`gnome-terminal-server.service` because that unit
declares `PartOf=graphical-session.target` under a name in no `app-*.scope` family.

Fixing the pattern list makes the symptom go away and leaves the inversion in place. **A better set
would only have hidden it.**

### 6.3 What "do what mutter does" means here

mutter asks nobody. It does not read the unit graph, does not stop units, does not consult
inhibitors (those are gnome-session's, and they gate the *dialog*, not the exit) — it quits the main
loop and `wl_display_destroy_clients` closes the sockets. It does not even declare `STOPPING=1`:
`shell_util_sd_notify` sends `READY=1` and nothing else (`gnome-shell/src/shell-util.c:774-778`).
Every unit stop in a GNOME logout is systemd's, resolved from `PartOf=graphical-session.target`, and
it happens because the shell **answered immediately**.

The tempting reading was that the drain was fine and only its *ask* was wrong — that a better set of
units (a `ConsistsOf` query, a PID-to-unit map) would fix the gnome-terminal case. It would have, and
that is what makes it the trap: it fixes the symptom by making our substitute for the teardown a
better substitute, and leaves the inversion in place for the next unit that does not fit whatever
rule we picked. **Twice now the answer to "which units?" has been one more entry.**

So the whole thing came out:

- `SessionDrain`, `DRAIN_TIMEOUT`, `DRAIN_SETTLE`, `poll_session_drain` and the drain timer.
- The confirm drain. `confirm_end_session` emits `Confirmed*` at once, as `endSessionDialog.js` does;
  gnome-session's teardown then runs, and systemd stops the real set by declaration.
- `stop_app_scopes` and its pattern list.
- `sd_notify(STOPPING=1, EXTEND_TIMEOUT_USEC=…)`, which existed only to buy the drain room against
  our own `TimeoutStopSec`.
- SIGTERM, the `Quit` action and the exit-confirm dialog all go back to `stop_signal.stop()`.

**What we give up** is real and worth naming: an app that is slower than our unwind loses its socket
mid-shutdown, which is §1's race, and we no longer paper over it. What we get back is that the set of
units asked is no longer ours to get right, and the failure mode is GNOME's rather than one of our
own invention. The three drain conformance tests went with it; what still pins the behaviour that
matters is `a_launched_app_can_be_asked_to_quit` (§2.2), because hearing the SIGTERM is now the whole
of an app's protection.

**If the race is ever worth addressing again, address the race** — the ordering gap in §6.1, which is
in systemd's job graph and would be fixed by an `After=` that makes the app stops complete before the
shell's, not by the compositor waiting on the side.


---

## 7. Open: is GNOME actually losing this race?

Raised 2026-08-03, immediately after §6 landed. **Still open**, but no longer speculative: the two
leading candidates have now been measured, one is dead and one turned out to be a deficit of ours.

§1 says apps survive under GNOME because mutter's unwind happens to be slower than theirs, and calls
that luck. That reads badly the moment you say it out loud: OBS shuts down cleanly under mutter
routinely, and "the compositor consistently loses a race" is a weak explanation for something that
consistent. Something is probably biasing it, and if so it is worth knowing what, because it may be
something we could have rather than something we must do without.

**Method note, because it is reusable.** Both answers below came from an `LD_PRELOAD` shim over
`wl_display_destroy_clients` / `wl_client_destroy` / `wl_display_destroy` that timestamps each call,
run against our headless compositor *and* against a real nested `gnome-shell --wayland --no-x11`
(hosted inside a headless niri, on a private bus via `dbus-run-session`, so no seat is touched).
That is an apples-to-apples A/B of mutter and us on the same machine with the same clients, and it
settled in minutes what days of journal archaeology had got backwards. **Reach for it before
reasoning about shutdown ordering again.**

### 7.1 The unwind-duration candidate is dead, and its measurement was contaminated

The first guess was that **mutter takes longer to unwind than we do**, so against a fixed head start
its margin is systematic rather than lucky. It had a number: 1.264 s on kov's session against our
0.262-0.577 s. **Retracted — that logout crashed.**

```
22:46:14.468  ERROR:../src/shell-app.c:1776:shell_app_dispose: assertion failed
22:46:14.468  Bail out!
22:46:15.607  Main process exited, code=dumped, status=6/ABRT
22:46:15.630  Stopped org.gnome.Shell@user.service
```

Most of that second is the **core dump**. It is also the only real-GNOME logout in the journal
window — every other `org.gnome.Shell@user.service` stop on this machine belongs to a gsrs manager,
which is us. So there is no valid measurement of mutter's clean unwind here, and the comparison it
was drawn from compared a crash to a teardown.

**The candidate is dead on structure anyway**, which is the more useful half.
`wl_display_destroy_clients` is called from `meta_context_dispose` (`meta-context.c:802`) *before*
`meta_display_close` and `meta_backend_destroy`:

```c
g_signal_emit (context, signals[PREPARE_SHUTDOWN], 0);
g_clear_object (&priv->service_channel);
meta_wayland_compositor_prepare_shutdown (...)          /* wl_display_destroy_clients */
meta_display_close (...)
g_clear_object (&priv->wayland_compositor);
g_clear_pointer (&priv->backend, meta_backend_destroy); /* DRM, GPU, monitors — the slow part */
```

So however long mutter's unwind is, **its clients are disconnected at the beginning of it.** Their
grace period from the compositor ends there, not at process exit. A slow unwind buys them nothing;
the slow part happens after they are already gone.

### 7.2 `wl_display_destroy_clients`: we already call it, and we are 12x worse than mutter

Three assumptions were checked rather than argued, because two of them were wrong.

**What the call does — it is not a protocol goodbye.** There is no "server is going away" event in
core Wayland; nothing exists to send. `wl_display_destroy_clients` walks the client list calling
`wl_client_destroy` (`wayland/src/wayland-server.c:1629-1653`), and that function
(`:998-1028`) is:

```c
wl_priv_signal_final_emit(&client->destroy_signal, client);
wl_client_flush(client);                                       /* :1014 push queued events */
wl_map_for_each(&client->objects, remove_and_destroy_resource, NULL);
wl_event_source_remove(client->source);
close(wl_connection_destroy(client->connection));              /* :1018 close the fd */
```

So it *is* the server closing the socket, exactly as one would assume — the only thing it adds over
the fd dying with the process is the flush of already-queued events and the server-side destructors.
Worth knowing that **`wl_display_destroy` does not do this** (`:1305-1329` frees the display and its
listening sockets and never touches `client_list`), so the call is genuinely separate and
load-bearing.

**Whether we call it — yes, and it is not obvious from our source.** We build smithay with
`use_system_lib`, which selects wayland-backend's `sys` backend, whose `Drop for State<D>` calls the
real `wl_display_destroy_clients` (`wayland-backend-0.3.16/src/sys/server_impl/mod.rs:429-436`). Our
`Display` is owned by the calloop source at `src/niri.rs:6621`, so it drops with `event_loop` when
`main` returns. **Verified, not assumed**, with an `LD_PRELOAD` shim over
`wl_display_destroy_clients` / `wl_client_destroy` / `wl_display_destroy` on a headless run with a
weston-terminal attached: SIGTERM produced `destroy_clients ENTER → wl_client_destroy →
LEAVE → wl_display_destroy → process exit`. So there was never a mechanism to add.

**How we compare — badly.** The same shim on a *nested real gnome-shell* (`--wayland --no-x11`,
inside a headless niri, private bus), SIGTERMed the same way:

```
                      SIGTERM → destroy_clients      → process exit
mutter  (5 clients)   67.9 ms                        +48.9 ms  =  116.8 ms
ours    (1 client)     5.6 ms                        + 0.3 ms  =    5.9 ms
```

**mutter keeps its clients' sockets alive ~12x longer than we do.** The earlier claim in this
document — that because mutter calls `destroy_clients` at the *top* of its dispose and we get it at
the *bottom* of ours, we are the more generous one — was wrong in the way that matters. What a client
experiences is the absolute time from its SIGTERM to its socket closing, and mutter's 68 ms of
pre-destroy work (gjs shutdown, the `PREPARE_SHUTDOWN` signal, `service_channel` teardown) is longer
than our entire exit. Where `destroy_clients` sits *within* each teardown is irrelevant; the length
of the part before it is the whole story.

Two caveats on the numbers: ours is a **debug** build (release would be faster, widening the gap),
and the nested mutter has no DRM backend — but `meta_backend_destroy` runs *after* `destroy_clients`,
so a real session cannot shorten the 68 ms, and a real session's heavier pre-shutdown work would
likely lengthen it. Treat 68 ms as a floor for mutter and 5.6 ms as optimistic for us.

### 7.3 What is left

Two candidates are now measured rather than guessed, and they point opposite ways:

- **We are 12x quicker to close the socket than mutter** (§7.2). If the compositor's presence during
  an app's shutdown matters at all, that is a real deficit and it is ours, not a race we lose.
- **But 68 ms is small next to the 341 ms head start** the apps already have from §1, so it is hard
  to believe it is the difference between a clean shutdown and a crash report.

Which leaves the possibility that the premise was wrong all along: **apps may not need the compositor
to shut down cleanly.** OBS's crash flag is written from its own SIGTERM handler, which touches no
display. If that is the story, then every "unclean shutdown" we recorded was the blocked signal mask
(§2.2) — an app that never got the SIGTERM and was SIGKILLed — and neither the race nor the 68 ms
ever mattered.

**The experiment that would settle it**, and it is cheap now that the harness exists: run OBS under a
nested gnome-shell and under our headless compositor, SIGTERM each compositor with OBS up, and read
OBS's own log for whether it saved state. That distinguishes "needs the compositor" from "needs the
signal" directly, instead of inferring either from journal timing.

**If it turns out the 68 ms does matter**, the fix is not a drain: it is to notice that our teardown
is 6 ms because we do almost nothing, and to decide deliberately what should happen between the
signal and the socket closing — which is a design question about our own shutdown, not about waiting
for clients.
