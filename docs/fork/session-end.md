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

The app scopes and `org.gnome.Shell@user.service` land in the **same stop transaction with no
`After=` between them**. Start order is shell → `gnome-session-initialized.target` →
`gnome-session.target` → `graphical-session.target`, so stop order reverses to three inert target
jobs and then the shell — sub-millisecond hops. Nothing waits for an app scope before killing the
compositor; both sides just carry `TimeoutStopSec=5s`.

Measured on our own session (journal, 2026-08-03 14:10:15; `systemd[57776]` is the gsrs user
manager, and that Epiphany was launched by us):

```
14:10:15.837  Stopping app-gnome-org.gnome.Epiphany.desktop-190833.scope...   SIGTERM to the app
14:10:16.175  Stopped target graphical-session.target
14:10:16.179  Stopping org.gnome.Shell@user.service...                        SIGTERM to us, +341 ms
14:10:16.441  epiphany[190833]: Lost connection to Wayland compositor.        we went first
14:10:16.457  Stopped org.gnome.Shell@user.service.
14:10:16.741  Stopped app-gnome-…-190833.scope                                app done, +562 ms after
```

**Read that fourth line.** The app got a 341 ms head start and needed 903 ms; our unwind took 262 ms.
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

### 2.1 Scopes carry their session membership explicitly

`start_transient_scope` (`src/utils/spawning.rs`) now sets `PartOf=graphical-session.target`,
`Description` and `TimeoutStopUSec` itself, on both scope prefixes.

Our `app-gnome-*` scopes already inherited all of that from gnome-session's drop-in — the prefix
was matched on purpose, and `DropInPaths` on a live scope confirms it. Setting it directly does two
things that inheritance does not: it covers `app-niri-*` (the `spawn` path, which matches no
drop-in and got nothing), and it stops the property that makes logout work from resting on a file
shipped by the package this fork intends to replace.

> Reading these properties back has a trap. `systemctl show` on a unit that no longer exists
> answers with **stub defaults** rather than an error — `PartOf=`, `TimeoutStopUSec=45s`,
> `Description=<the unit name>` — which reads exactly like "the drop-in never applied". Check
> `Transient=yes` / `DropInPaths=` before believing a negative result. This is most likely what is
> behind the `PartOf=` empty claim in `overview-port.md` §4 gap 2.

### 2.2 The compositor outlives its clients

Every path that ends the compositor now goes through **`Niri::begin_session_drain`** instead of
`stop_signal.stop()`: the termination signals (`utils::signals`) and the `Quit` action. Rather than
unwinding, we stay in the event loop — still dispatching, still rendering, still flushing — until
the last client window is gone or `DRAIN_TIMEOUT` (5 s) expires.

- **The oracle is the window count**, `layout.with_windows` — plus a settle. Zero windows starts a
  500 ms `DRAIN_SETTLE` rather than ending the drain, because unmap is not "done": a toolkit that
  *handles* SIGTERM destroys its toplevels near the start of shutdown and keeps using the socket
  afterwards (GL contexts, dmabuf feedback, the registry). Leaving at unmap would reopen the same
  `Broken pipe` on a shorter fuse, and precisely for the apps that shut down gracefully rather than
  dying where they stand. An app killed outright never unmaps at all — the compositor sees the
  disconnect, so the two coincide. The settle restarts if a window reappears and `DRAIN_TIMEOUT`
  still bounds the whole thing.

  The strictly correct oracle is live client *connections*, but it needs an allowlist for the
  clients that are ours: xwayland-satellite holds a connection for as long as it runs, so waiting
  on it would push every logout with an X app to the full timeout. Not worth the machinery for the
  residual risk; noted here so the option is on record.
- **The poll runs after `flush_clients`** in `refresh_and_flush_clients`, plus once on a deadline
  timer. The first point is what makes the drain end the instant the last window goes; the timer is
  what wakes an otherwise idle desktop to give up.
- **`sd_notify(STOPPING=1, EXTEND_TIMEOUT_USEC=…)`** buys the budget against our own unit's
  `TimeoutStopSec`, which would otherwise be counting the whole time. Sent **only on the stopping
  path**, never on the dialog drain: `STOPPING=1` moves the unit into deactivating and arms that
  timeout, so declaring it while we intend to keep running would have systemd kill us mid-session
  if gnome-session's teardown ever stalled. A SIGTERM folding into a confirm drain sends it then.
- **Timing out is a warning that names the apps.** Which client ignored a five-second SIGTERM is
  the only thing worth knowing from that line.
- **`DRAIN_TIMEOUT` cannot usefully exceed the scope's `TimeoutStopSec`, and both are 5 s because
  we set both.** systemd starts its own five-second clock on the same `StopUnit` that begins the
  drain, and SIGKILLs the app when it expires. Measured 2026-08-03: firefox was asked at
  `22.8280`, we gave up at `27.800`, systemd killed it at `27.8277` — the two clocks land 27 ms
  apart by construction. So the drain buys an app the *compositor's* company for up to 5 s (against
  the 341 ms head start in §1, which is the win), but it cannot buy it more life than its scope
  has. Raising one number alone does nothing; `TimeoutStopUSec` in `start_transient_scope` and
  `DRAIN_TIMEOUT` in `src/niri.rs` move together or not at all.

Apps are asked to go the way GNOME asks them — SIGTERM, by stopping their scopes
(`spawning::stop_app_scopes`, `ListUnitsByPatterns` + `StopUnit`). There is deliberately **no
`xdg_toplevel.close` sweep**: mutter has none, and sending one would put "save changes?" dialogs in
front of a logout the user has already confirmed. GNOME's answer to unsaved work is the inhibitor
protocol, not a close sweep. What we add over GNOME is only the waiting.

**A flatpak app is not in the scope we started for it**, which is why the patterns include
`app-flatpak-*` — a prefix we never create. We do call `start_app_scope` and get
`app-gnome-<id>-<pid>.scope`; then `flatpak run` moves the real processes into a scope of its own
and exits, ours goes empty, and `CollectMode=inactive-or-failed` collects it. By logout the only
unit holding the app is flatpak's. Measured 2026-08-03 with OBS: our scope was started at
`16:14:11.500`, flatpak's 70 ms later, and at `16:14:22.827` the stop went to firefox, Epiphany and
a gsd helper — OBS was not asked to quit until the `graphical-session.target` teardown reached it
at `16:14:27.835`, five seconds into a drain that had already timed out naming it. Nothing else
covered it: it is the same class of unit (`/usr/lib/systemd/user/app-flatpak-.scope.d/` ships the
same `PartOf=graphical-session.target` + `TimeoutStopSec=5s` drop-in), so all the pattern changes is
*when* it is asked — at the start of the drain instead of after it.

This is also the argument for the pattern list over a registry of what we launched, sharper than
the one in the code comment: a registry would have recorded the scope we started, which is exactly
the one that no longer holds the app.

On an externally driven logout, `stop_app_scopes` joins stop jobs systemd has already queued and is
a no-op. On the `Quit` and dialog paths it is the only thing that asks the apps at all — which is
why it is the one systemd call here that blocks. The rest are fire-and-forget on a thread; this one
is followed immediately by a poll that can reach process exit within the same call, and a detached
thread would be killed before it had finished connecting to the bus.

### 2.3 The end-session dialog answers late

Confirming the dialog (or letting its countdown expire) no longer emits `Confirmed*` straight away.
`begin_end_session_drain` records the answer, drains, and emits it when the apps are gone.

The answer is what makes gnome-session start the teardown that SIGTERMs us — so emitting it with
apps still up hands the ordering back to the job graph that has no ordering in it. Draining first
means gnome-session's phases only ever run on a desktop with no app windows left: the deterministic
version of the head start above — which, as §1 shows, was not a margin we were winning anyway.

This drain does **not** stop us. gnome-session still drives the teardown exactly as before; its
SIGTERM just lands on an empty session, where the follow-up drain has only its settle left to pay.
Expect a ~500 ms tail on every logout for that reason: it is `DRAIN_SETTLE`, not a stall. A SIGTERM
arriving *during* a confirm drain folds into it rather than restarting the clock or being dropped.

### 2.4 Not in a session, not draining

`is_session_instance` gates the whole thing. Nested and headless runs quit at once, as they always
did — there is no session teardown to outlive, and a five-second wait on every `Quit` would be a
dev-loop tax paid for nothing. It is also what keeps the headless tests fast; the three conformance
tests in `src/tests/gnome.rs` set the flag by hand.

### 2.5 An app must not inherit the compositor's signal mask

The drain had never once succeeded before 2026-08-03, and this is why: **every app launched from
the dash, grid or search started life unable to receive SIGTERM.**

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

**Measured on the seat afterwards**, the same OBS that had been SIGKILLed three logouts running:

```
16:50:44.358482  asked systemd to stop app-flatpak-…obsproject…scope
16:50:44.359444  OBS: ==== Shutting down ====                        ← 0.96 ms
16:50:44.918195  clients are gone, ending the session                ← 559 ms
```

0.96 ms against GNOME's 0.8 ms, a drain that ends on `clients are gone` instead of timing out, and
no "Crash or unclean shutdown detected" on the next start.

---

## 3. Known gaps

- **Terminal-spawned clients have no scope**, so `stop_app_scopes` cannot reach them and nothing
  SIGTERMs them at logout. The drain still waits for their windows; they are only asked to leave
  when the socket dies, as under GNOME. (A flatpak app started from a terminal is the exception —
  `flatpak run` makes it a scope regardless of who launched it, so `app-flatpak-*` now reaches it.)
- **`DBusActivatable` apps are unreachable from the drain** — see §4. The bus restart that ends
  them is triggered by our own exit, so waiting moves it out of reach rather than into it. Measured
  with a keyring prompt open: 10 s of drain, then `Broken pipe` anyway.
- **`--session` under a non-systemd init** (dinit) drains with no way to stop the scopes, so a
  `Quit` there waits out the full budget before the warning: the drain is gated on
  `is_session_instance` while `stop_app_scopes` is gated on `IS_SYSTEMD_SERVICE`. Nobody runs that
  configuration today.
- **A withdrawal during a confirm drain is ignored, and now has a window to arrive in.** Confirming
  stops the app scopes immediately but holds `Confirmed*` for up to ~5.5 s. If gnome-session
  withdraws the request in that window (`EndSessionDialogToNiri::Close`, `src/niri.rs`),
  `EndSession::close` is already a no-op — the dialog was taken by `confirm` — so the drain goes on
  to answer a request that no longer exists, on a desktop whose apps have already been told to quit.
  Before the drain this window was ~0. The user did confirm, and gnome-session withdrawing after a
  confirm is not something we have seen, so this is recorded rather than fixed; the fix is for
  `Close` to cancel `session_drain_confirm`, which needs a decision about what the half-stopped
  session should then look like.
- **A stuck app can cost two budgets — seen 2026-08-03, and it cost 10.07 s.** If a confirm drain
  times out, gnome-session's subsequent SIGTERM starts a fresh drain that gives what is left
  another 5 s. Measured with firefox, Epiphany and OBS up: drain 1 `22.829 → 27.800`, SIGTERM at
  `27.894`, drain 2 `27.915 → 32.897`. Worth reading *why* before adding state to fix it: the only
  app in drain 2 was OBS, and OBS was in it because nothing had asked it to quit yet — the flatpak
  scope fix above is what that logout actually needed. Carrying the deadline across is still the
  fix for the general case; it is not obviously the fix for the case we have.

  With the fix in, measured again 2026-08-03: one logout took **5.6 s and ended on
  `clients are gone`** — the first clean drain we have recorded — and the next took the full 10 s on
  `gcr-prompter`, which is the D-Bus-activated case above and would not have been helped by either
  budget. So the second budget is now only ever spent on clients the drain cannot influence, which
  is an argument for capping the *total* rather than for carrying the deadline.
- **Inhibitors are still parsed and discarded.** `EndSessionDialog.Open` takes a list of inhibitor
  object paths (`src/dbus/gnome_session.rs:59`) and the dialog does not show them, so an app with
  unsaved work has no way to say so. This is the piece that would let apps push back on a logout,
  and it is GNOME's designed answer to the problem a close sweep would create; it belongs with the
  dialog work, not here.
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

**The drain does *not* cover it, and the reason is worth keeping.** An earlier revision of this
section argued that it did — the drain keeps us serving Wayland for 5 s, the bus restart lands
~425 ms after we are told to stop, so the app gets to close its own socket. That reasoning has a
hole in it: the restart is not at a fixed offset from our *SIGTERM*, it is downstream of our
*exit*. `gnome-session-restart-dbus.service` is pulled in by `gnome-session-shutdown.target`, which
`org.gnome.Shell@user.service` triggers through `OnSuccess=`. **Every second the drain waits pushes
the bus restart back by the same second**, so it can never arrive during the drain — the deadline
moves with us.

Measured 2026-08-03, a logout with a keyring password prompt open (`gcr-prompter`, in
`dbus-:1.2-org.gnome.keyring.SystemPrompter@0.service`):

```
16:26:51.495  drain 2 starts (after drain 1 timed out on it)
16:26:56.501  clients did not exit within 5s: gcr-prompter
16:26:56.598  gcr-prompter: Error reading events from display: Broken pipe   ← the yank
16:26:56.641  Stopped org.gnome.Shell@user.service
16:26:56.645  Started gnome-session-restart-dbus.service                      ← 47 ms too late
```

Ten seconds of drain, both budgets spent, and the client still went out on `Broken pipe` — the
exact failure the drain exists to prevent, in the one class of client it cannot help. So this is a
**gap, not a footnote**: a D-Bus-activated client that only exits when the bus goes is unreachable
from here on *every* path, `Quit` included, because we are what the bus restart is waiting for.

What would actually fix it is asking gnome-session to restart the bus before we exit rather than
after, or stopping the `dbus-:1.x-*` unit for a client whose window is holding the drain. The
second is tempting and wrong as stated — those units are not ours, and a blanket pattern would take
out session services we still need — but a *targeted* stop, of a unit we can tie to a window that
is holding the drain open, is a different proposition and has not been explored.

Do not read "the app has no scope" as "the app is unprotected"; read it as "the drain has no
lever on this one", which is the conclusion this section now exists to record.
