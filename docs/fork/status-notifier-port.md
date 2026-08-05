# StatusNotifierItem / AppIndicator support

**Status: S1–S5 landed — indicators register, draw in all three icon forms, and clicking one opens
its client's remote menu.** What is left is S6 (the click ladder, scroll, activation tokens) and
S7 (seat validation against real clients).

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
| Icon decode (pixmap → texture, extra theme search path) | `src/render_helpers/icon.rs` + its rasterizer worker |

The menu is the part with real design weight. Every popup we have today
(`quick_settings.rs`, `app_menu.rs`, `input_source_menu.rs`, `a11y_menu.rs`) is built from
statically-known structure; `app_menu.rs`'s `Row` / `RowAction` pair is the closest existing
shape. A remote tree needs a **data-driven menu widget** — rows built from a model at runtime,
with submenus, checkmarks, radio groups and per-row icons. Per the toolkit-first rule that is a
`widget::` primitive, not a one-off inside the indicator; `app_menu.rs` and
`input_source_menu.rs` should end up built on it too.

---

## Slices

### S1 — The watcher and the item registry ✅ (`efcf7245`)
Own `org.kde.StatusNotifierWatcher`, export the object, accept registrations, track item
lifetime by bus-name owner, emit the four signals. No UI. Gets us the property that makes clients
stop hiding their tray affordance, which several check at startup.

**The test seam** (decided here; S5's tests inherit it). The headless
harness runs no session bus, and standing one up inside a parallel test binary collides with the
dconf/`dbus-run-session` isolation trap. Every other bus feature here is pinned at a **channel
seam** — the corpus drives the model with the messages the bus layer would have delivered
(`src/tests/server.rs`, the gdm/accounts pattern). Do the same: the wire code stays thin enough to
eyeball, the registry/model logic gets the tests, and the bus itself is proved in S7.

### S2 — The item model and one panel icon ✅ (`56f6d138`)
Proxy `org.kde.StatusNotifierItem`, resolve `IconName` through our existing icon lookup, place
it in the panel's right box left of the status cluster, honour `Status` (`Passive` hides —
`indicatorStatusIcon.js:321`). Themed icons only.

Two things that were part of *this* slice and are easy to defer by accident: **readiness gating**
(see the trap below — an item is not showable the moment it registers) and **capability
introspection**. The extension calls `org.freedesktop.DBus.Introspectable.Introspect` on the item
once at setup and records whether `Activate` and `XAyatanaSecondaryActivate` exist
(`appIndicator.js:446-457`); S6's click ladder is built on that answer, so the probe belongs here
with the rest of the item model.

Landed with two extra divergences worth naming. **We do not require a menu to show an item** — the
extension's `isReady` is `hasNameOwner && id && menuPath` (`appIndicator.js:476-486`) and
`menuPath` is null for `/NO_DBUSMENU`, so an activate-only item never appears in it at all; we gate
on `Id` alone. And the **liveness probe landed here rather than later**: a property read answering
one of the six fatal errors retires the item, which is what catches the Electron case below.

The cluster is **one panel role** (`ROLE_APP_INDICATORS`) with per-item hit boxes inside it, the
shape `quickSettings` already uses for its icons — the panel addresses roles by `&'static str`, and
indicators are dynamic in count. It leads the right box, so a session that accumulates a dozen
grows leftward and never displaces the clock or the status cluster.

### S3 — Pixmap and out-of-theme icons ✅ (`990d60c0`)
`IconPixmap` is `a(iiay)`: ARGB32, **network byte order**, one entry per size. Pick the smallest
entry ≥ the target size, else the largest available (`pixmapsUtils.js:32-67`), then byte-swap to
RGBA (`:17-30`) and upload as a texture — this bypasses themed lookup entirely.
`IconThemePath` adds a per-item directory to the search path. **The estimate above was wrong about
where this lands**: `IconCache` is the *symbolic* cache, and none of this is symbolic. A file — an
absolute path sent as a name, or a name found inside the client's own directory — is an app-supplied
picture, which is what `ImageSource::File` + `ImageCache` already exist for, worker and eviction
included. So no structural change to `IconCache` was needed; the per-item search became a small
resolver on the watcher's side (`icon_from_name`), which is also where the filesystem check belongs,
off the frame thread.

What `IconCache` did grow is `texture_for_buffer`: pixels the *caller* owns still need
upload-once-then-reuse and the dead-device guard, or every frame pays a submit + fence-wait.

`ImageFit::Contain` settles the multiload question with an existing verb — the strip keeps its
aspect and is letterboxed in the slot, no special case. And a themed name stays tinted to the panel
foreground while a pixmap or file keeps the client's colours: it is the app's artwork, not a glyph.

### S4 — The data-driven menu widget ✅ (`93eb3be4`)
`widget::Menu`: rows from a model, separators, submenus, checkmark/radio state, per-row icon,
insensitive rows, keyboard navigation. Rendered inside the existing `PanelPopover` as a new
`PopoverContent` variant. No D-Bus in this slice — drive it from a fixture tree so the widget is
testable on its own, then port `app_menu.rs` onto it as proof the abstraction holds.

**Settled: submenus expand in place** (decision 2026-08-04), as GNOME's `PopupSubMenuMenuItem`
does (`popupMenu.js:1308`; shipped in `status/keyboard.js` and `status/network.js`) and as the
extension renders them (`dbusMenu.js:590-591`, `:616-620`). One surface, one grab, and it matches
every other menu in the shell — which matters more here than usual, because this widget now backs
`app_menu` too. A cascading second popover is what these clients' native toolkits do, but it needs
a grab spanning two surfaces and per-level edge-clamping.

**Scrolling landed after the fact** (`7398db88`): a menu grows to its caller's cap — the monitor's
work area, as `panelMenu.js:168-186` does — and scrolls past it, with the keyboard dragging the view
along. What is still missing is a **scrollbar**: a long menu gives no hint there is more below it.
GNOME's overlay scrollbars only appear on hover, so this is a smaller gap than it sounds, but it is
open.

### S5 — The DBusMenu client ✅
`GetLayout` at depth −1 for the initial tree, `AboutToShow` before opening any submenu **and on
the root**, `Event("clicked")` on activation, `opened`/`closed` on menu open/close
(`dbusMenu.js:664-673`), and live re-fetch on `LayoutUpdated` / `ItemsPropertiesUpdated`. Wire
S4 to it. `AboutToShow`'s reply is not decoration: `needUpdate = true` means re-fetch the layout
before showing, and the reply must be accepted in both signatures (see the traps).

Landed as `src/dbusmenu.rs` (the model: `MenuNode`, mnemonic stripping, `to_entries`),
`src/dbus/dbusmenu.rs` (the wire: `GetLayout`, `Event`, `AboutToShow`, the two update signals) and
`ui::indicator_menu::IndicatorMenu` (the popover content). Two things the plan did not say:

- **The menu opens empty.** A client's rows are a round trip away, and `AboutToShow` may be a
  second, so the box is on screen before its contents are. Everything downstream — the height cap,
  the anchor — has to survive a layout arriving *after* the open.
- **The close needs a reconciler, not a call site.** A popover is dismissed from half a dozen
  places (Escape, an outside click, another menu opening, the overview, a lock), and a client that
  never hears `closed` stops answering `AboutToShow`. `State::reconcile_indicator_menu` runs every
  cycle and acts only on a difference, so no dismissal path has to remember.

### S6 — Interaction semantics
Click, middle-click, right-click, scroll (see the click ladder below), plus
`ProvideXdgActivationToken`. The token goes out before `Activate` *and* before a menu item's
`clicked` event (`dbusMenu.js:677-684`) — a menu entry that opens a window needs it just as much.
The extension already mints a genuine token, by faking an `AppInfo` to get a startup-notify id out
of `create_app_launch_context` (`appIndicator.js:766-773`); our advantage is only that we mint
ours natively, with no fake `AppInfo` in the way.

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

**An item that keeps its bus name is not necessarily alive.** Electron apps *remove the SNI
object* when hiding the indicator and leave the bus name owned, so owner-tracking alone leaves a
ghost icon in the panel forever — from exactly the client class that motivates this whole port.
The extension probes `Get(Status)` after a 10 s delay and destroys the item on any of
`NameHasNoOwner`, `ServiceUnknown`, `UnknownObject`, `UnknownInterface`, `UnknownMethod`,
`UnknownProperty` (`appIndicator.js:623-666` — the "hey electron!" comment). Ping is deliberately
*not* used: it is unreachable inside snap confinement.

**An item is not showable the moment it registers.** Clients routinely register before exporting
their properties. The extension gates on `Id` *and* `Menu` being present, retrying three times a
second apart before giving up (`appIndicator.js:406-408`, `:498-529`). Showing an icon straight
off registration produces items that render broken or never populate.

**`Menu == "/NO_DBUSMENU"` means there is no menu** (`appIndicator.js:576-580`), and some items
have no menu at all. The click ladder needs defined behavior for a menu-less item — not a popover
containing nothing.

**`Status` must default to `Passive` before the first fetch** (`appIndicator.js:115-116`), or
every item flashes into the panel during startup and some never should have appeared at all.

**Icon names are not always icon names.** `indicator-sensors` sends an absolute *path*
(`appIndicator.js:1247-1265`, the `name[0] === '/'` branch); others send `foo.png`, which must
have its extension stripped before lookup. Both are out-of-spec and both are common.

**Some "icons" are not square.** `indicator-multiload` sends a wide strip and expects it drawn at
its natural aspect ratio; the extension special-cases `width >= height * 1.5`
(`appIndicator.js:1174`) — note this applies only to icons that resolved to a *file path* in a
user-writable area, not to pixmaps or themed lookups. Decide deliberately whether we letterbox or
scale-to-fit, and pin it.

**`SecondaryActivate` has an Ayatana-only spelling.** Try `XAyatanaSecondaryActivate(timestamp)`
first, fall back to `SecondaryActivate(x, y)` on `UnknownMethod`, and cache which one worked
(`appIndicator.js:817-840`). The same probe-and-cache applies to `Activate` itself
(`:804-810`) and to `AboutToShow` (`dbusMenu.js:508-528`) — clients omit methods they declare.

**Dropbox needs `AboutToShow(0)` before the root menu is valid** (`dbusMenu.js:893-894`). Call it
on the root, not just on submenus — and it replies with `()` instead of the specified `(b)`, so
the reply handling must accept an empty tuple as "yes, re-fetch" (`dbusMenu.js:508-524`).

**Labels carry GTK mnemonics.** `_Quit` must have the underscore stripped for display
(`dbusMenu.js:735` — which is a first-match, non-global replace; doing it properly means handling
`__` as a literal underscore too).

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
  and instead builds a click ladder around a double-click timeout: on an `Activate`-capable item,
  a single primary click waits `doubleClickTime` and *then* opens the menu, while a double click
  calls `Activate` (`indicatorStatusIcon.js:375-445`; an item with no menu items skips the wait,
  `:437-442`). The delay exists only because the extension can't tell a menu-first item from an
  activatable one. `ItemIsMenu` is exactly that signal, and honouring it is what Plasma does —
  which is what SNI clients are actually written and tested against.

  Our ladder: left click opens the menu when `ItemIsMenu` is set or the item exposes no
  `Activate`, otherwise `Activate`; right click always opens the menu; middle click is
  `SecondaryActivate`; scroll forwards `Scroll` with an axis name. `Activate` answering
  `UnknownMethod` at call time demotes the item to menu-first for the rest of its life, the way
  the extension's `supportsActivation` does (`appIndicator.js:804-810`).

  **The class this serves worse**: a client that declares `Activate` in introspection, leaves
  `ItemIsMenu` unset, and then no-ops the call. Under the extension the user gets the menu after
  the timeout; under us, left click appears to do nothing and the menu is only reachable by right
  click. It cannot be detected in principle — a successful no-op is indistinguishable from a
  successful activation. Accepted, but S7 checks for it explicitly (Dropbox is the likely
  candidate).
- **A real activation token.** `ProvideXdgActivationToken` with a token we minted natively, rather
  than the extension's fabricated `AppInfo` (`appIndicator.js:766-773`).
- **No tooltips.** A choice, not a gap: `widget::Tooltip` exists (`src/ui/widget.rs:685`) and the
  dash, window previews and the screenshot UI use it. The extension disables `ToolTip` in its IDL
  and we follow, because the property is `(sa(iiay)ss)` — icon, title and *markup* body — and
  honouring it properly means a second rich-text surface for content we don't control. Revisit
  when a client is demonstrably unusable without it.
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
- Submenus: inline-expanding (one surface, like the extension and like GNOME's own menus) or
  cascading child surfaces (like Plasma, and like every toolkit menu these clients were designed
  against)? Decides the shape of `widget::Menu` and how far the grab has to reach. See S4.
