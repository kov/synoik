# End-session dialog port

What gnome-shell's `js/ui/endSessionDialog.js` does, what we have, and what is left. This is the
document `session-end.md` §3 defers the inhibitor question to; it owns the dialog's *content*,
while `session-end.md` owns what happens after the user confirms.

Reference: `~/Projects/gnome-shell` at 50.3 (`2d5e9a29b`).

---

## 1. The flow is closed already

Worth stating first, because it decides whether any of the content work below is reachable.

```
quick settings "Power Off…"            src/ui/quick_settings.rs:687
  → SessionRequest::PowerOff           src/end_session.rs
  → org.gnome.SessionManager.Shutdown  src/dbus/gnome_session.rs:request_session_action
  → gnome-session decides
  → EndSessionDialog.Open(1, …) back on us   src/dbus/gnome_session.rs
  → EndSession::open + the dialog widget     src/synoik.rs:6641
  → ConfirmedShutdown / Canceled             src/synoik.rs:14411
```

gnome-session calls `Open` back on the `org.gnome.Shell` bus name, so the dialog is the shell's,
not gnome-session's — everything below is ours to render. The corpus drives the real entry point
(`end_session_dialog_open_confirm_and_cancel`, `src/tests/gnome.rs:7646`).

The keyboard shortcut path (`src/input/mod.rs:2585-2591`) goes through the same
`request_session_action`, so there is one mechanism, not two.

## 2. What we have

`src/end_session.rs` (pure lifecycle: open / confirm / tick / cancel / close, plus the countdown)
and `src/ui/end_session_dialog.rs` (the surface: fixed 400×190 box, title, counting-down
description, Cancel + one action button, open/close animation, focus and hit-testing).

Three dialog types, matching the `Open` wire values: logout 0, shutdown 1, restart 2.

## 3. Offline updates

**GNOME 50 does not talk to PackageKit.** `b47e3763e` (2026-01-13, first in 50.0) replaced
`org.freedesktop.PackageKit.Offline` on the system bus with **`org.gnome.Software.OfflineUpdates`
on the session bus**. Per the fork tenet we implement 50's way; the older PackageKit path is not a
capability we are giving up, it is the same feature through a different door.

Interface (`gnome-shell/data/dbus-interfaces/org.gnome.Software.OfflineUpdates.xml`), marked
unstable by gnome-software itself:

| Method | Signature | Notes |
|---|---|---|
| `GetState` | `→ s` | `"none"` \| `"prepared"` \| `"scheduled"` |
| `Cancel` | | cancels a prepared update; no-op when none |
| `SetAction` | `s →` | `"reboot"` \| `"shutdown"`; may fail `NOT_SUPPORTED` |

Live on a Fedora 44 seat: `busctl --user call org.gnome.Software /org/gnome/Software/OfflineUpdates
org.gnome.Software.OfflineUpdates GetState` → `s "none"`. gnome-software owns the name, so the
service is activatable and the interface is real, not aspirational.

### 3.1 Behaviour to reproduce

- **Proxy lazily, on open** (`:222-225`, `:289-305`). GNOME deliberately does *not* create the
  proxy at startup — gnome-software's systemd unit is delayed so login is cheap, and constructing
  the proxy early would activate it. Ours must be built when the dialog opens, not before.
- **Absence is not an error.** Constructing a GDBus proxy for a missing service succeeds; GNOME
  detects it by `g_name_owner === null` (`:300`). zbus proxies are lazy the same way, so we need an
  explicit name-owner check. **Any error, missing name, or unrecognized state string ⇒ treat as
  unavailable ⇒ the dialog behaves exactly as it does today.** A power-off must never be blocked or
  broken by an update query.
- **Checkbox visibility** (`:715-718`): visible iff the type has `checkBoxText` — shutdown and
  restart only, never logout — *and* state is `prepared` or `scheduled`. Label: "Install pending
  software updates".
- **Checked by default**, unless the battery is low (`:718`, §4).
- **Title swaps when checked** (`_sync:341`): `subjectWithUpdates` — "Install Updates & Restart",
  "Install Updates & Power Off".
- **On confirm** (`:469-497`) — the subtle part:

  | Type | Checkbox | Call | Signal emitted |
  |---|---|---|---|
  | restart | checked | `SetAction("reboot")` | `ConfirmedReboot` |
  | shutdown | checked | `SetAction("shutdown")` | **`ConfirmedReboot`** if the call succeeded, else `ConfirmedShutdown` |
  | either | unchecked | `Cancel()` | unchanged |
  | UPDATE_RESTART | — | `SetAction("reboot")` | `ConfirmedReboot` |

  The shutdown row is the one to remember: **"Power Off" can legitimately put `ConfirmedReboot` on
  the wire.** Offline updates are applied during a reboot, and gnome-software powers the machine
  off afterwards — so the reboot is an implementation detail of the power-off the user asked for.
  When `SetAction` returns `NOT_SUPPORTED` the backend cannot do that, and we must *not* rewrite the
  signal, or the user gets a reboot they did not ask for. This is worth corpus tests on all four
  combinations; read as a bug otherwise.

- **`UPDATE_RESTART`, a fourth *presentation* type** (`:684-687`). gnome-session never sends type 3;
  the shell promotes RESTART→UPDATE_RESTART itself when gnome-software is present and the state is
  `scheduled`. Content: subject "Restart & Install Updates", button "Restart & Install", no
  checkbox. **`EndSessionType` must stay a three-variant wire enum** — this is a separate
  presentation field, not a fourth `from_u32` case, or an unexpected `3` on the wire becomes a
  reboot.

### 3.2 Slices

1. **`widget::CheckBox`** — a shared toolkit control, not an inline shape in the dialog (see
   CLAUDE.md, toolkit-first). Spec cached in `gnome-style-reference.md` §check-box, from
   `_check-box.scss`: 14px `check-symbolic` glyph, 6px radius, 2px border white@15% (hover 20%,
   active 30%); checked = accent fill, `#fff` glyph, transparent border; focus ring inset 2px
   accent@35% on a 7px box with 2px padding; label spacing 0.8em. Hover/checked/focus modelled on
   `widget::Button`.
2. **Offline-updates client** — `src/dbus/gnome_software.rs` (zbus, session bus, `dbus` feature) and
   a pure state model. Fail-safe per §3.1.
3. **Dialog integration** — checkbox row, `subjectWithUpdates` titles. The box is fixed at 400×190
   (`ui/end_session_dialog.rs:44-45`); the checkbox needs the dialog re-measured against the
   reference rather than a guessed delta (see the `visual-bug: anchor then measure` rule).
4. **Confirm-path semantics** — `SetAction`/`Cancel`, and the shutdown→reboot rewrite, with the
   four-combination corpus tests.

## 4. Backlog

Everything gnome-shell's dialog has that ours does not. None of it blocks §3; all of it is real.

- **`UPDATE_RESTART` presentation type** (§3.1). Deferred with the rest of §3 beyond the checkbox.
- **Battery gate** — `_isBatteryLow()` (`:313-316`): discharging *and* under 30% ⇒ the update
  checkbox starts unchecked. We have `system_status::BatteryStatus.percentage`
  (`src/system_status.rs:295`), but "discharging" is currently approximated from
  `icon_name.contains("charging")`; this wants UPower's real `State` property. **Note the half-state
  if this lands alone:** GNOME pairs the gate with a visible warning label (below), and a checkbox
  that silently starts unchecked with no explanation is worse than either end.
- **Low-battery warning label** — "Low battery power: please plug in before installing updates"
  (`:260-266`), shown per `_shouldShowLowBatteryWarning`. Pairs with the gate above.
- **Inhibitor application list** — the gap `session-end.md` §3 defers here, and the one that is a
  missing *feature* rather than a limit. `Open` hands us inhibitor object paths
  (`src/dbus/gnome_session.rs:59`) and we discard them, so an app with unsaved work cannot say so.
  GNOME loads each as a `GnomeSession.Inhibitor`, resolves it to an app (`findAppFromInhibitor`,
  `:153`), and lists only those whose flags include `InhibitFlags.LOGOUT` *and* which resolve to a
  real app — services and non-logout inhibitors are dropped (`_onInhibitorLoaded`, `:571-591`).
  Dead inhibitors are expected and tolerated (`:158`). Section title: "Some applications are busy or
  have unsaved work".
- **Other-sessions list** — "Other users are logged in", up to `MAX_USERS_IN_SESSION_DIALOG` = 5
  (`:140`), from logind via `_loadSessions`. Shown for shutdown and restart, not logout
  (`showOtherSessions`).
- **User name in the logout title** — `subjectWithUser` / `descriptionWithUser` ("Log Out %s"),
  from AccountsService once `is_loaded` (`_sync`). We always use the impersonal form.
- **Boot-loader-menu restart** — `_canRebootToBootLoaderMenu` (`:216`, `:285-288`) and the alternate reboot
  button it gates. Logind's `RebootToBootLoaderMenu`.
- **Countdown rounding** — `_roundSecondsToInterval(…, 10)` in `_sync`: GNOME rounds the displayed
  seconds to a 10-second interval rather than counting every second. We count every second.
