# Accessibility — a milestone of its own

**Status: A1 done (the toggle surface), everything it toggles unbuilt.**

This is a *milestone*, not a chrome slice. R4 in `panel-status-port.md` delivered the
`ATIndicator` menu — ten switches that write ten gsettings keys — and that is the **control
panel for a feature we have not built**. Turning on Zoom, Screen Reader, Screen Keyboard,
Visual Alerts or the four keyboard filters currently does nothing at all, because every one
of them is a subsystem the compositor owes and none of them exist here yet. Treating the
rest as "more panel work" would badly misjudge the size: each row below is days-to-weeks,
several are their own protocol surface, and one (§A8) is the part that actually decides
whether a blind user can use this desktop.

**Reference-first.** Every claim below cites GNOME 50.1 in `~/Projects/gnome-shell` or
`~/Projects/mutter`. Re-read the cited file before implementing — do not work from this
summary alone.

---

## Where the work lives

Accessibility in GNOME is split across three layers, and we own all three:

| Layer | GNOME | Us |
|---|---|---|
| Settings surface | `js/ui/status/accessibility.js` | ✅ `src/ui/a11y_menu.rs` (R4) |
| Shell-internal features | `js/ui/magnifier.js`, `js/ui/keyboard.js`, St | ⬜ §A2, A3, A5 |
| Compositor/input features | mutter `meta-keyboard-a11y.c`, `bell.c`, `meta-a11y-manager.c` | ⬜ §A4, A6, A7 |
| Our own UI's a11y tree | AT-SPI via St's ATK bridge | ⬜ §A8 — **the real one** |

---

## Slices

### A1 — The toggle surface ✅ (`1626e16b`)
The `ATIndicator` indicator + menu. Writes the canonical keys; consumes none of them.
See `panel-status-port.md` R4.

### A2 — High Contrast + Large Text in our own chrome
The two rows that already do *something* today, because GTK apps read the keys directly —
but our shell chrome ignores both, so the panel and popovers stay identical while every app
changes. GNOME routes both through `StSettings`:
- `high-contrast` is an `StSettings` property (`src/st/st-settings.c:35,350-354`), and the
  shell reloads a **separate stylesheet variant** on it — `_getStylesheet(name.replace('.css',
  '-high-contrast.css'))` (`js/ui/main.js:461-463`), re-run from
  `St.Settings.get().connect('notify::high-contrast', _loadDefaultStylesheet)` (`main.js:176`).
  For us that is a second theme in the widget layer, not a filter — it lands naturally on the
  cssparser cascade ([[widget-layer]] slice B1) and should probably wait for it.
- `text-scaling-factor` multiplies the realized base font. We already have exactly the right
  seam: `crate::ui::base_font_pt` / `set_base_font_pt`, which every point size is a ratio
  against ([[gnome-visual-style-reference]]). This slice is small and worth doing early.

**Do A2 first** — it is the one slice that makes two existing switches honest.

### A3 — Magnifier (the Zoom row)
`org.gnome.desktop.a11y.magnifier` (`js/ui/magnifier.js:25`), `Magnifier`
(`magnifier.js:131`). Shell-internal in GNOME: a zoom region composited over the stage, with
mouse/focus/caret tracking, lens vs full-screen modes, crosshairs, brightness/contrast/
saturation filters and colour-blindness modes (the schema has ~30 keys — `brightness-*`,
`contrast-*`, `color-saturation`, `caret-tracking`, …). For us it is a render-path feature on
the owned Vulkan renderer, which makes it cheaper than GNOME's but not small. `StSettings`
also exposes `magnifier-active` (`st-settings.c:388-392`), which other chrome reads.

### A4 — Keyboard filters (Sticky / Slow / Bounce / Mouse Keys)
Compositor-side, and **not** something libinput does for us: mutter implements the state
machines itself in `src/backends/native/meta-keyboard-a11y.c`, fed from
`org.gnome.desktop.a11y.keyboard` through `meta-input-settings.c:1297-1305` (which maps each
key to a `META_A11Y_*` flag, including the beep variants we don't surface). Sticky Keys also
needs the modifier-latch feedback GNOME shows. Mouse Keys turns the numpad into a pointer.
This is a self-contained input-path slice against our own seat handling.

### A5 — On-screen keyboard (the Screen Keyboard row)
`screen-keyboard-enabled` (`js/ui/keyboard.js:25`), `Keyboard` (`keyboard.js:1174`) — a full
shell-side OSK with layouts, suggestions and input-method integration. Already on the roadmap
independently (STRATEGY §6 Phase 3, "OSK"); the a11y row is just another way to enable it.

### A6 — Visual bell (the Visual Alerts row)
`org.gnome.desktop.wm.preferences visual-bell` + `visual-bell-type`, handled in mutter's
`src/core/bell.c:208-250` (flash the focused frame or the whole screen, with rate limiting).
Small, and a good candidate to pair with A4.

### A7 — The a11y D-Bus surface + dwell click
- mutter exports `org.freedesktop.a11y.KeyboardMonitor` and a `PointerLocator`
  (`src/backends/meta-a11y-manager.c:86-89,245-336`: grab/ungrab/watch/unwatch keyboard, key
  grabs, query pointer). This is what Orca drives. Per STRATEGY §3.9, cosmic-comp already
  serves the keyboard half — worth reading before writing ours.
- Dwell click is `panel-status-port.md` R3 plus `org.gnome.desktop.a11y.mouse`
  (`dwell-click-enabled`, `dwell-mode`, `dwell-time`, `secondary-click-*`) — the indicator is
  panel work but the *behavior* is input work, so it belongs here.

### A8 — Our own UI is actually accessible (AT-SPI / AccessKit)
**The one that matters most, and the one nothing above delivers.** Every slice up to here is
about helping the user drive *other* apps; this is whether a screen reader can read our panel,
overview, dialogs and menus at all. GNOME gets it free because St widgets are ATK objects. We
have a hand-rolled toolkit that emits textures, so we have *nothing* — our entire shell is
invisible to Orca today.

STRATEGY §3.9 already picked the approach: emit an **AccessKit** tree from the widget layer,
exposed over AT-SPI2 via `accesskit_unix`. That means every `widget::` type needs a role, a
name and a bounding box, which is a toolkit-wide change and the reason this is a milestone
rather than a slice. **The Screen Reader row in A1 is a lie until this exists** — it enables
Orca, and Orca then finds nothing to read in the shell.

### A9 — Screen reader launch
`screen-reader-enabled` starts Orca in a GNOME session (gsd/session-side, not shell-side). The
row currently just writes the key; confirm whether anything on this system acts on it, and
whether we need to do the launching ourselves.

---

## Suggested order

1. **A2** — makes two shipped switches honest, and the text-scaling half is nearly free.
2. **A4 + A6** — self-contained compositor/input work, no new protocol surface.
3. **A8** — the big one; start it as soon as the widget layer's cascade work (B1) settles,
   because roles/names want to live next to the theme node.
4. **A3**, **A5**, **A7**, **A9** — each independently schedulable.

## Deliberately not here
- The `QuickToggle` a11y classes at `accessibility.js:149-327` are **GDM-only**
  (`js/gdm/loginDialog.js:429-438`) — they belong to a login-screen milestone, not this one.
- Newton (the compositor-mediated a11y relay for *client* trees) stays deferred per
  STRATEGY §3.9: it is experimental upstream and even GNOME's own shell still uses legacy
  AT-SPI under it.
