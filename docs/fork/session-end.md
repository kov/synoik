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

The app scopes and `org.gnome.Shell@wayland.service` land in the **same stop transaction with no
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
14:10:16.741  Stopped app-gnome-…-190833.scope                                app done, +562 ms after
```

341 ms of head start; the app needed 903 ms. It survived only because our unwind happened to take
longer than the 562 ms it still needed. Lose that coin flip and the client is mid-shutdown when its
display goes away — EPIPE, which GTK3 raises as a fatal `g_error` and Firefox turns into an
`ANOM_ABEND`. That is the crash in `overview-port.md`.

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

Apps are asked to go the way GNOME asks them — SIGTERM, by stopping their scopes
(`spawning::stop_app_scopes`, `ListUnitsByPatterns` + `StopUnit`). There is deliberately **no
`xdg_toplevel.close` sweep**: mutter has none, and sending one would put "save changes?" dialogs in
front of a logout the user has already confirmed. GNOME's answer to unsaved work is the inhibitor
protocol, not a close sweep. What we add over GNOME is only the waiting.

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
version of the 341 ms of luck above.

This drain does **not** stop us. gnome-session still drives the teardown exactly as before; its
SIGTERM just lands on an empty session, where the follow-up drain completes immediately. A SIGTERM
arriving *during* a confirm drain folds into it rather than restarting the clock or being dropped.

### 2.4 Not in a session, not draining

`is_session_instance` gates the whole thing. Nested and headless runs quit at once, as they always
did — there is no session teardown to outlive, and a five-second wait on every `Quit` would be a
dev-loop tax paid for nothing. It is also what keeps the headless tests fast; the three conformance
tests in `src/tests/gnome.rs` set the flag by hand.

---

## 3. Known gaps

- **Clients with no scope are still unprotected, and cost the full budget.** A `DBusActivatable`
  app is launched by the bus, so `app_system.rs` skips scope creation for it (`pid == 0`) and its
  unit is `dbus-:1.x-org.foo@N.service` — matching neither `APP_SCOPE_PATTERN`. Same for anything
  spawned from a terminal. On the `Quit` and dialog paths nothing SIGTERMs them, so the drain waits
  its whole 5 s and then exits over them anyway: the original bug, preserved for this app class,
  plus a constant tax on those logouts. GNOME has the identical hole (its `dbus-.service.d` drop-in
  sets only `Slice=`, no `PartOf=`), and on an *externally* driven logout they are equally exposed
  under both. Closing it means either widening the patterns — dangerous, `dbus-*` covers services
  we still need — or `xdg_toplevel.close` as a fallback for windows whose client we could not
  reach, which is the divergence deliberately not taken here.
- **`--session` under a non-systemd init** (dinit) drains with no way to stop the scopes, so a
  `Quit` there waits out the full budget before the warning: the drain is gated on
  `is_session_instance` while `stop_app_scopes` is gated on `IS_SYSTEMD_SERVICE`. Nobody runs that
  configuration today.
- **A stuck app can cost two budgets.** If a confirm drain times out on it, gnome-session's
  subsequent SIGTERM starts a fresh drain that gives the same app another 5 s — worst case a ~10 s
  logout. Carrying the deadline across would fix it; not worth the state until it is seen.
- **Inhibitors are still parsed and discarded.** `EndSessionDialog.Open` takes a list of inhibitor
  object paths (`src/dbus/gnome_session.rs:59`) and the dialog does not show them, so an app with
  unsaved work has no way to say so. This is the piece that would let apps push back on a logout,
  and it is GNOME's designed answer to the problem a close sweep would create; it belongs with the
  dialog work, not here.
- **No `RegisterClient` participation.** We do not register as a session client, matching
  gnome-shell. Since GTK4 dropped registration this protocol is dead for apps too, so there is
  nothing to gain until something starts speaking it again — the new `xx_session_v1` toplevel
  session protocol is about *restoring* windows, not quitting them.
