# StatusNotifierItem / AppIndicator support

**Status: not started.** This document is the plan.

## Why this one has no GNOME reference

`grep -ri 'statusnotifier\|appindicator'` over `~/Projects/gnome-shell` and `~/Projects/mutter`
(both 50.3) returns **nothing**. GNOME removed the legacy XEmbed tray in 3.26 and never adopted
the KDE StatusNotifierItem spec; its position is that apps should use notifications, background
portals and the app's own window instead. So this is not a port in the usual sense — there is no
`js/ui/` file to be faithful to.

It is, all the same, the single most-installed GNOME extension, because a large class of clients
expose *no other* tray path: Electron apps, Steam, Telegram, Discord, Nextcloud, KeePassXC,
Syncthing, Qt apps generally. On a desktop with no support they either lose their affordance
entirely or (worse) run with no visible way to reach them.

**This is therefore an explicit, approved divergence from GNOME**, and the conformance corpus
here pins *our* chosen behavior rather than ported behavior. The reference is the extension:

| Reference | Where |
|---|---|
| `gnome-shell-extension-appindicator` v64 (ubuntu/rgcjonas), shell 45–50 | `~/.local/share/gnome-shell/extensions/appindicatorsupport@rgcjonas.gmail.com` |
| `org.kde.StatusNotifierWatcher` / `…Item` IDL | `<ext>/interfaces-xml/StatusNotifier*.xml` |
| `com.canonical.dbusmenu` IDL | `<ext>/interfaces-xml/DBusMenu.xml` |
| Rust prior art (read, don't depend) | `system-tray` 0.8.7 — async SNI + DBusMenu client on zbus |

`ksni` 0.3.6 is the *server* side (publishing an item) and is not relevant to us.
`system-tray` is the right shape but carries a tokio runtime and its own ownership model; every
other bus surface here is a hand-rolled `src/dbus/*.rs` on our blocking zbus connection, and this
should match.

---

## The three wire surfaces

**1. `org.kde.StatusNotifierWatcher`, which we host.** We own the well-known name and export
`/StatusNotifierWatcher`. Clients call `RegisterStatusNotifierItem(s)`, and we publish
`RegisteredStatusNotifierItems`, `IsStatusNotifierHostRegistered`, `ProtocolVersion` plus four
signals. The extension refuses `RegisterStatusNotifierHost` outright with `NOT_SUPPORTED`
(`statusNotifierWatcher.js:262-267`) — there is exactly one host, and it is the shell.

**2. `org.kde.StatusNotifierItem`, which each client hosts.** Properties: `Category`, `Id`,
`Title`, `Status`, `Menu` (an object path), `ItemIsMenu`, `IconThemePath`, and — for each of
normal/attention/overlay — an `…IconName` *and* an `…IconPixmap` (`a(iiay)`). Methods:
`Activate`, `SecondaryActivate`, `ContextMenu`, `Scroll`, `ProvideXdgActivationToken`. Changes
arrive as `New*` signals rather than `PropertiesChanged`, so the properties must be re-fetched
by hand on each one (`appIndicator.js:178-183`).

**3. `com.canonical.dbusmenu`, which each client hosts.** This is the hard part and it is a
*remote menu tree*, not a list: `GetLayout(parentId, depth, props) → (revision, (ia{sv}av))`
recursing into children, `GetGroupProperties`, `Event(id, "clicked"|"opened"|"closed"|"hovered",
data, ts)`, `AboutToShow(id) → needUpdate` for lazily-filled submenus, and the
`LayoutUpdated` / `ItemsPropertiesUpdated` / `ItemActivationRequested` signals. Per-item
properties we must honour (`dbusMenu.js:75-90`): `visible`, `enabled`, `label`, `type`
(`standard` | `separator`), `children-display` (`submenu`), `icon-name`, `icon-data` (raw PNG
bytes), `toggle-type` (`checkmark` | `radio`), `toggle-state`.

---

## Where the work lands

| Piece | File |
|---|---|
| Watcher export, item registry, item proxies | `src/dbus/status_notifier.rs` (new) |
| DBusMenu client — tree fetch, property cache, event send | `src/dbus/dbusmenu.rs` (new) |
| Item model the UI renders from (icon, status, menu tree) | `src/status_notifier.rs` (new) |
| Panel indicators + hit-testing | `src/ui/panel.rs` — new `PanelItem` roles in `PanelBox::Right` |
| The menu itself | `src/ui/widget.rs` + a new `PopoverContent` variant |
| Icon decode (pixmap → texture, extra theme search path) | icon cache in `src/synoik.rs` / `app_system.rs` |

The menu is the part with real design weight. Every popup we have today
(`quick_settings.rs`, `app_menu.rs`, `input_source_menu.rs`, `a11y_menu.rs`) is built from
statically-known structure; `app_menu.rs`'s `Row` / `RowAction` pair is the closest existing
shape. A remote tree needs a **data-driven menu widget** — rows built from a model at runtime,
with submenus, checkmarks, radio groups and per-row icons. Per the toolkit-first rule that is a
`widget::` primitive, not a one-off inside the indicator; `app_menu.rs` and
`input_source_menu.rs` should end up built on it too.

---

## Slices

### S1 — The watcher and the item registry
Own `org.kde.StatusNotifierWatcher`, export the object, accept registrations, track item
lifetime by bus-name owner, emit the four signals. No UI: assert registration through
`synoik-ipc` and a synthetic client in the corpus. Gets us the property that makes clients stop
hiding their tray affordance, which several check at startup.

### S2 — The item model and one panel icon
Proxy `org.kde.StatusNotifierItem`, resolve `IconName` through our existing icon lookup, place
it in the panel's right box left of the status cluster, honour `Status` (`Passive` hides —
`indicatorStatusIcon.js:321`). Themed icons only.

### S3 — Pixmap and out-of-theme icons
`IconPixmap` is `a(iiay)`: ARGB32, **network byte order**, one entry per size. Pick the smallest
entry ≥ the target size, else the largest available (`pixmapsUtils.js:32-67`), then byte-swap to
RGBA (`:17-30`) and upload as a texture — this bypasses themed lookup entirely.
`IconThemePath` adds a per-item directory to the search path, which our icon loader currently has
no way to express; that API change is part of this slice.

### S4 — The data-driven menu widget
`widget::Menu`: rows from a model, separators, submenus, checkmark/radio state, per-row icon,
insensitive rows, keyboard navigation. Rendered inside the existing `PanelPopover` as a new
`PopoverContent` variant. No D-Bus in this slice — drive it from a fixture tree so the widget is
testable on its own, then port `app_menu.rs` onto it as proof the abstraction holds.

### S5 — The DBusMenu client
`GetLayout` at depth −1 for the initial tree, `AboutToShow` before opening any submenu,
`Event("clicked")` on activation, `opened`/`closed` on menu open/close
(`dbusMenu.js:664-673`), and live re-fetch on `LayoutUpdated` / `ItemsPropertiesUpdated`. Wire
S4 to it.

### S6 — Interaction semantics
Click, middle-click, right-click, scroll (see the trap on the click ladder below), plus
`ProvideXdgActivationToken` before `Activate` — we already mint xdg-activation tokens, so unlike
the extension we can hand out a real one and let the app raise itself properly.

### S7 — Live validation on the seat
The corpus can prove the wire and the widget; it cannot prove that Steam's menu is usable. Run
the seat session against a spread of real clients — one Electron (Nextcloud), one Qt/KDE
(KeePassXC), one Ayatana (Syncthing), one that is notoriously non-conforming (Dropbox) — and
check icon, menu, activation and teardown for each.

---

## Traps, all cited from the extension

**The register argument is either a bus name or an object path.** Ayatana-patched apps send a
path and mean "my own bus name, this path"; KDE apps send a bus name and mean the well-known
`/StatusNotifierItem`. Dispatch on a leading `/` and fall back to the message sender
(`statusNotifierWatcher.js:207-235`). A watcher that assumes one form silently drops half the
ecosystem.

**A well-known name must be resolved to a unique name before you can track its death.** The
extension resolves via `GetNameOwner` at registration (`:217-224`) and keys items on the unique
name; otherwise name-owner transfer and item identity come apart.

**Do not destroy an item the instant its owner vanishes.** Apps that re-register during their own
restart flicker the panel; the extension waits 500 ms and re-checks (`:104-116`).

**`Status` must default to `Passive` before the first fetch** (`appIndicator.js:115-116`), or
every item flashes into the panel during startup and some never should have appeared at all.

**Icon names are not always icon names.** `indicator-sensors` sends an absolute *path*
(`appIndicator.js:1247-1265`, the `name[0] === '/'` branch); others send `foo.png`, which must
have its extension stripped before lookup. Both are out-of-spec and both are common.

**Some "icons" are not square.** `indicator-multiload` sends a wide strip and expects it drawn at
its natural aspect ratio; the extension special-cases `width >= height * 1.5`
(`appIndicator.js:1174`). Decide
deliberately whether we letterbox or scale-to-fit, and pin it.

**`SecondaryActivate` has an Ayatana-only spelling.** Try `XAyatanaSecondaryActivate(timestamp)`
first, fall back to `SecondaryActivate(x, y)` on `UnknownMethod`, and cache which one worked
(`appIndicator.js:817-840`). The same probe-and-cache applies to `Activate` itself
(`:804-810`) and to `AboutToShow` (`dbusMenu.js:508-528`) — clients omit methods they declare.

**Dropbox needs `AboutToShow(0)` before the root menu is valid** (`dbusMenu.js:893-894`). Call it
on the root, not just on submenus.

**Labels carry GTK mnemonics.** `_Quit` must have the underscore stripped for display
(`dbusMenu.js:735`).

**Property updates arrive fast enough to matter.** The extension rate-limits icon rebuilds to
30 ms (`appIndicator.js:43`); some clients repaint a progress icon per frame. Our equivalent
budget is a texture upload per change, which is worse — this needs coalescing from the start,
and per `docs/fork/frame-submit-discipline.md` those uploads belong in the frame's own submit,
never a `run_commands` of their own.

**Menu revisions can race the user.** `LayoutUpdated` while the menu is open must rebuild without
losing the open submenu or the pointer's row.

---

## Divergences we are choosing

- **`ItemIsMenu` is honoured; the extension ignores it.** The extension never reads the property
  and instead builds a click ladder around a double-click timeout: single primary click waits
  `doubleClickTime` and *then* opens the menu, a double click calls `Activate`
  (`indicatorStatusIcon.js:375-445`). That delay is felt on every single click, and it exists
  only because the extension can't tell a menu-only item from an activatable one. `ItemIsMenu`
  is exactly that signal. Our ladder: left click opens the menu when `ItemIsMenu` or when the
  item exposes no `Activate`, otherwise `Activate`; right click always opens the menu; middle
  click is `SecondaryActivate`; scroll forwards `Scroll` with an axis name. Clients that set
  neither get menu-on-left-click, which is what users expect and what the timeout was
  approximating anyway.
- **A real activation token.** `ProvideXdgActivationToken` with a token we actually minted, so
  activation raises the window instead of flagging it as demanding attention.
- **No tooltips.** The extension disables `ToolTip` in its IDL and so do we; we have no
  tooltip surface at all yet.
- **No `XAyatanaLabel` text beside the icon.** A per-item text label in the panel is an Ubuntu
  extension to the spec and would fight the panel's width budget. Revisit only if a real client
  proves unusable without it.
- **No `tray-pos` knob.** The extension exposes left/center/right placement; there is no config
  file here (see the memory note) and GNOME wouldn't have one. Right box, fixed.

## Out of scope

- **Legacy XEmbed X11 tray icons** (`trayIconsManager.js`). It needs an X11 systray selection
  owner inside Xwayland and a reparenting host for foreign windows; the client set that has
  *only* this path is close to empty in 2026. Not planned.
- **`AttentionMovieName`** — animated attention icons. Nothing sets it.
- **Brute-force bus scanning** (`statusNotifierWatcher.js:148-205`): the extension shells out to
  a bus analyzer to find items that registered before it loaded, because an extension can be
  enabled mid-session. We are the compositor and own the name before any client starts, so the
  window it papers over does not exist here.

## Open questions

- Does the item cluster get its own `PanelBox` sub-box with its own spacing, or does it share
  `.panel-status-indicators-box` with the quick-settings icons? Affects hit-testing and the
  panel width budget when a user has eight of them.
- Ordering: registration order, `Category`, or `XAyatanaOrderingIndex`? Registration order is
  non-deterministic across boots, which reads as icons that move around.
