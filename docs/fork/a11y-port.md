# Accessibility — a milestone of its own

**Status: A1 (the toggle surface), A2, A3, A4, A5, A6, A7 and A9 done — A5 without its word
suggestions, which wait on the IBus panel; A8 (our own UI readable by a screen reader) is
unbuilt.**

This is a *milestone*, not a chrome slice. R4 in `panel-status-port.md` delivered the
`ATIndicator` menu — ten switches that write ten gsettings keys — and that is the **control
panel for features we had not built**. Zoom, Screen Reader and Screen Keyboard still do
nothing at all, because each is a subsystem the compositor owes and none exists here yet. Treating the
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
| Shell-internal features | `js/ui/magnifier.js`, `js/ui/keyboard.js`, St | ✅ §A2, A3, A5 |
| Compositor/input features | mutter `meta-keyboard-a11y.c`, `bell.c`, `meta-a11y-manager.c` | ✅ §A4, A6, A7 |
| Our own UI's a11y tree | AT-SPI via St's ATK bridge | ⬜ §A8 — **the real one** |

---

## Slices

### A1 — The toggle surface ✅ (`1626e16b`)
The `ATIndicator` indicator + menu. Writes the canonical keys; consumes none of them.
See `panel-status-port.md` R4.

### A2 — High Contrast + Large Text in our own chrome ✅
The two rows that already do *something* today, because GTK apps read the keys directly —
but our shell chrome ignores both, so the panel and popovers stay identical while every app
changes. GNOME routes both through `StSettings`:
- `high-contrast` is an `StSettings` property (`src/st/st-settings.c:35,350-354`), and the
  shell reloads a **separate stylesheet variant** on it — `_getStylesheet(name.replace('.css',
  '-high-contrast.css'))` (`js/ui/main.js:461-463`), re-run from
  `St.Settings.get().connect('notify::high-contrast', _loadDefaultStylesheet)` (`main.js:176`).
  ✅ Ours is one process-wide switch (`crate::ui::high_contrast`, published with the base font
  by `GnomeSettings::publish_style`) that the palette in `widget::style` reads: the values the
  high-contrast sheet moves are functions there, and `Painter::hc_inset` draws its 1px
  `$hc_inset_color` outline inside buttons, entries, cards, quick toggles and icon buttons.
  Shadows go and the overview plate stays dark. Values come from compiling both sheets with
  `sassc` and diffing them per selector. A surface that keeps a hand-picked local colour
  rather than a `widget::style` one does not follow it yet; the cascade
  ([[widget-layer]] B1) is what makes that impossible to miss.
- ✅ `text-scaling-factor` multiplies the realized base font
  (`GnomeSettings::realized_base_font_pt`). mutter folds the factor into the font DPI
  (`meta-settings.c:184-199`) and St scales every `pt` and `em` length by it, not `px`
  (`st-theme-node.c:1350-1373`) — the same set our base drives through `pt_to_px`, `em` and
  `scaled_px`.

### A3 — Magnifier (the Zoom row) ✅
`src/magnifier.rs`, a port of `Magnifier` and `ZoomRegion` (`js/ui/magnifier.js`), on from
`screen-magnifier-enabled` and configured by `org.gnome.desktop.a11y.magnifier`:
- **The zoom region**: `mag-factor` (a change eased over 100 ms), the five `screen-position`s
  with the region's 2px accent border off full screen, `lens-mode`, `scroll-at-edges`, and
  proportional / push / centred `mouse-tracking`. The frame a display would draw is rendered into
  an offscreen and drawn rescaled and cropped to the view, over `SystemBackground`'s colour.
- **The pointer** stays where it is, and so do clicks; it is drawn magnified where the zoom puts
  it, never on a cursor plane (`render_helpers::off_plane`), and the system pointer hides over the
  view. Under a nested session the pointer goes in the frame on a magnified display.
- **Crosshairs** and the **colour effects** (`invert-lightness`, `color-saturation`, per-channel
  `brightness-*` / `contrast-*`) — the effects in one pass of `mag_effects.frag`, in the order
  Clutter stacks them, over a second offscreen of the region only while one is on.
- **Focus and caret tracking** (`src/focus_caret_tracker.rs`): an AT-SPI client over plain zbus
  that registers the focus and caret events with the registry while the zoom wants them and asks
  the emitting widget for its extents, as `FocusCaretTracker` and `_convertExtentsToScreenSpace`
  do. GTK4's text view emits no caret move as you type, only on navigation, so typing alone does
  not move the view — in GNOME either.
- The shortcuts (`<Alt><Super>8`, `<Alt><Super>equal`/`minus`) are gsd-media-keys', through our
  accelerator grabs.

**Divergence: one region per display.** GNOME's region magnifies the whole stage as one surface;
here each display magnifies its own content, the one under the pointer following it and the others
keeping their last view.

### A4 — Keyboard filters (Sticky / Slow / Bounce / Mouse Keys) ✅
`src/input/keyboard_a11y.rs`, a port of mutter's `src/backends/native/meta-keyboard-a11y.c`, fed
from `org.gnome.desktop.a11y.keyboard` as `meta-input-settings.c:1291-1340` reads it. libinput
reports keys untouched and xkbcommon has no AccessX, so the filters are the compositor's own,
between the device and the keymap:
- **Sticky Keys**: a modifier tapped alone latches onto the next key, tapped twice locks, again
  unlocks; a chord stays a chord; `stickykeys-two-key-off` turns it off on two modifiers at once.
  The latch lives in the xkb state, so clients and our keybindings see it — which needs
  `XkbContext::set_latched_and_locked_mods` from our smithay fork.
- **Slow Keys**, **Bounce Keys**, **Toggle Keys**, and **Mouse Keys** (keypad drives the pointer
  with Num Lock off, mutter's acceleration curve).
- **The `enable` shortcuts**: Shift ×5 toggles Sticky Keys, Shift held 8 s toggles Slow Keys,
  each writing its key back so the menu row follows.
- Every beep variant, each the system bell (`meta_bell_notify (display, NULL)`, A6).

**Divergence:** mutter filters after the key has updated the xkb state, so a Shift that Slow
Keys holds back still reaches the modifiers clients see. Ours filters before the keymap: a
swallowed key changes nothing, and an accepted slow key is delivered when accepted.

gnome-shell shows no latched-modifier indicator (mutter's `kbd-a11y-mods-state-changed` has no
listener). What it does show is `KbdA11yDialog` (`js/ui/kbdA11yDialog.js`), the "Sticky Keys
Turned On" confirmation after a shortcut (Shift ×5, Shift held, two-key-off) and never after a
settings change: `ui/kbd_a11y_dialog.rs`, on the shared `ui/message_dialog.rs`. It answers over the
lock screen too, since the shortcuts work there. `timeout-enable` /
`disable-timeout` are read by mutter and acted on nowhere.

### A5 — On-screen keyboard (the Screen Keyboard row) ✅
`src/osk/` (model) and `src/ui/osk.rs` (drawing, hit-testing), a port of `js/ui/keyboard.js`:
- **When it exists**: `screen-keyboard-enabled`, or touch mode with a touchscreen as the last
  device (`KeyboardManager._syncEnabled`; mutter's `update_touch_mode`).
- **When it shows**: a client re-enabling its text input while it already holds the input method
  (mutter's `enable_panel_v1` — GTK does it on a click), one of our entries taking the key focus,
  or a touch dragged up from the bottom edge (`ShellEdgeDragGesture`). It goes on the input
  method's focus-out, its hide key, or the focused window moving. Rest, slide and hide-panel
  timings are gnome-shell's.
- **Layouts** are gnome-shell's JSON models (`resources/osk-layouts`), with its purpose layouts,
  `-extended` for terminals and its group fallbacks. Levels, shift-once, the long-press latch,
  latching Ctrl/Alt, auto-capitalisation and title case from the surrounding text.
- **Keys**: a string goes in through the input method's commit when a field has it, otherwise
  as virtual key presses through the real pipeline (keyboard filters included) with Shift/AltGr
  held for the level, in mutter's order. A long press opens the key's other strings in a box
  pointer; the globe key the input sources (`LanguageSelectionPopup`); the emoji key the emoji
  page (sections, pages, a swipe or touchpad scroll between them, skin tones on long press).
- **The focused window** slides up out of the keyboard's way when the caret would be under it,
  and back after (`setCursorLocation`, `_animateWindow`).
- Drawn over the shell's chrome, the shield and dialogs included (`addTopChrome`), under only
  the pointer and its own popups; over the dock, whose edge reveal it holds off (decision:
  keyboard over dock).

**Not built: word suggestions.** gnome-shell fills the row from the IBus panel service's lookup
table (`ibusCandidatePopup.js:290-335`); we run `ibus-daemon --panel disable` as it does but have
no panel service, so neither the suggestions nor the desktop candidate popup exist. Porting the
panel service is its own slice.

**Divergences:**
- Delete holds BackSpace (a virtual key) rather than asking the input method to delete the
  surrounding text.
- It goes on the primary display, always (`keyboardIndex = primaryIndex`), the edge drag
  included; gnome-shell opens a drag's keyboard on the display it came from.
- An IBus source's keys follow `us`: without the engine's description its xkb layout is unknown.
- The language menu is no modal menu, as gnome-shell's is not, but it holds the click grab as any
  of our popovers does: a click elsewhere, the keyboard included, only closes it.
- The edge drag only claims the edge while there is a keyboard to pull; gnome-shell's gesture
  claims it regardless.
- The emoji page's long-press variants are the skin tones; gnome-shell groups by name prefix.
  An emoji commits the table's fully-qualified spelling, without gnome-shell's extra U+FE0F.
- A slide that the work area's titlebar clamp stops short comes back by as much as it went.

### A6 — The bell (the Visual Alerts row) ✅
`src/bell.rs`, a port of mutter's `src/core/bell.c` and the flashes it asks for
(`compositor.c:1350-1438`). We serve `xdg_system_bell_v1` (GTK4) and `gtk_shell1.system_bell`
(GTK3; `src/protocols/gtk_shell.rs`, at version 1 — later versions hand us duties we don't
perform). With `visual-bell` on, a bell flashes
the whole screen, or under `visual-bell-type = frame-flash` the ringing window (the whole screen
when the bell names no window). Rate limit: 500 ms between flashes, and a double flash after 3 s
of quiet.

**Divergence:** mutter's double flash ends at peak darkness and snaps off (Clutter's
`repeat_count = 2` is three passes); ours runs both flashes to the end.

The audible half is independent of the flash: with `audible-bell` on, every bell plays the
theme's `bell-window-system` (`org.gnome.desktop.sound theme-name`, silenced by `event-sounds`).
`src/sound_theme.rs` ports libcanberra's theme lookup and `src/sound_player.rs` plays it on a
PipeWire stream with `media.role = Notification` (canberra's `event` role, so GNOME Settings'
System Sounds volume applies).

The keyboard filters' beeps (A4) ring this same bell, as `meta_seat_native_bell_notify` does.

### A7 — The a11y D-Bus surface and pointer accessibility ✅
- ✅ `org.freedesktop.a11y.KeyboardMonitor` and `PointerLocator` on `org.freedesktop.a11y.Manager`
  (`src/dbus/freedesktop_a11y.rs`, mutter's `meta-a11y-manager.c`): watch, grab, the key grabs
  with the lone-tap/double-press rule for a grabbed modifier (`meta-seat-impl.c:520-568`), and
  QueryPointer with its one-shot PointerPositionChanged. Only the owner of
  `org.gnome.Orca.KeyboardMonitor` may call either, as in mutter; anyone else gets AccessDenied.
  **Divergence:** QueryPointer always names a window by pid. mutter names its AT-SPI bus name
  and path when GTK sent them over gtk-shell v7, and we serve v1 (`src/protocols/gtk_shell.rs`
  says why).
- ✅ Simulated secondary click and dwell click (`src/input/pointer_a11y.rs`, clutter's
  `clutter-input-pointer-a11y.c`), fed from `org.gnome.desktop.a11y.mouse`; the countdown pie
  at the pointer (`pointerA11yTimeout.js`, drawn by `sdf_pie.frag`) with the crosshair during a
  gesture; and the panel's `dwellClick` indicator (`src/ui/dwell_click_menu.rs`). mutter's
  gesture directions are kept as written: a stroke to the right reads as `left`.
  **Divergence:** the crosshair goes whenever a dwell timeout stops. mutter reports a gesture cut
  short as a dwell stopping, gnome-shell keeps the crosshair, and it stays until the next
  gesture runs out.

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

### A9 — Screen reader launch ✅ (nothing to build)
gnome-session launches Orca, not the shell. `/etc/xdg/autostart/orca-autostart.desktop` carries
`AutostartCondition=GSettings org.gnome.desktop.a11y.applications screen-reader-enabled`, and
gnome-session watches that key: turning the row on starts `orca.service` in the session, and
turning it off stops it (checked on the harness VM with orca 50.3 and gnome-session 50.1). What
Orca can *read* is A7 and A8.

---

## Suggested order

1. **A8** — the big one; start it as soon as the widget layer's cascade work (B1) settles,
   because roles/names want to live next to the theme node.
2. **The IBus panel service** — the candidate popup, and with it A5's word suggestions.

## Deliberately not here
- The `QuickToggle` a11y classes at `accessibility.js:149-327` are **GDM-only**
  (`js/gdm/loginDialog.js:429-438`) — they belong to a login-screen milestone, not this one.
- Newton (the compositor-mediated a11y relay for *client* trees) stays deferred per
  STRATEGY §3.9: it is experimental upstream and even GNOME's own shell still uses legacy
  AT-SPI under it.
