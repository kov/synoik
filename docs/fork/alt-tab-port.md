# Alt-Tab / Super-Tab — porting the GNOME switchers

Status: **agreed, slices 1-3 in progress**. Cited inventory of GNOME 50.3's `js/ui/altTab.js` (1146 lines) +
`js/ui/switcherPopup.js` (703) against what we ship today (`src/ui/mru.rs`, niri's MRU switcher).

## The headline: Super-Tab and Alt-Tab are two different UIs

We treat them as one. GNOME does not. `WindowManager._startSwitcher`
(`js/ui/windowManager.js:1670-1694`) picks a *different popup class* per keybinding, and the
session's defaults (read live from `org.gnome.desktop.wm.keybindings`) are:

| accel | key | popup | what it shows |
| --- | --- | --- | --- |
| `<Super>Tab` | `switch-applications` | `AppSwitcherPopup` | one item per **app**, 96px icons |
| `<Alt>Tab` | `switch-windows` | `WindowSwitcherPopup` | one item per **window**, 128px preview + 48px app icon |
| `<Alt>Above_Tab`, `<Super>Above_Tab` | `switch-group` | `AppSwitcherPopup` | same popup, opened *within the current app* |
| `<Alt>Escape` | `cycle-windows` | `WindowCyclerPopup` | **no list** — highlights each window in place |
| `<Alt>F6` | `cycle-group` | `GroupCyclerPopup` | ditto, within the current app |

`Above_Tab` is the key above Tab. **Correction to an earlier reading of this doc:** mutter does
*not* resolve it per layout. `add_keycodes_for_keysym` special-cases the fake `META_KEY_ABOVE_TAB`
to exactly `KEY_GRAVE + 8` and returns before it consults any keymap
(`src/core/keybindings.c:385-392`). Matching by *physical position* is what makes it
layout-independent: on AZERTY that key types `²`, and the binding still works because nothing ever
asks what it types. So "hardcoded backtick keycode" is the faithful port, not the shortcut.

## What we have

`src/ui/mru.rs` — niri's most-recently-used window switcher: window previews in a row, a border on
the selected one, a title underneath. One popup for every binding above, differing only by a
`MruFilter` (`All` / `AppId`) and an `MruScope` (`All` / `Output` / `Workspace`).

The keybindings are **partly** re-homed onto GNOME's schema (`src/gnome.rs:950-969`
`adopted_wm_keybindings`): `switch-windows` and `switch-applications` are adopted, each with its
`-backward` twin, but `switch-group`, `switch-group-backward`, `cycle-windows` and `cycle-group`
are **not** — so slices 4 and 6 carry schema-adoption work of their own, not just UI.

`GnomeKeyAction::SwitchApplications` already carries the note that this port exists to retire:

> GNOME groups by application and spans workspaces; we map it onto the window MRU switcher over
> all workspaces (no app grouping — accepted divergence for now).

So this is not a new divergence to negotiate — it is a debt already written down.

niri's own config surface (`synoik-config/src/recent_windows.rs`: `recent-windows` with
`debounce-ms`, `open-delay-ms`, `highlight`, `previews`, `binds`) is "niri's way" for a thing GNOME
does differently, so per the fork tenet it goes. GNOME's equivalents are fixed constants plus one
gsetting (below).

## The shared base — `SwitcherPopup` (`switcherPopup.js`)

Everything below sits on this, and it is where the *feel* lives:

- `POPUP_DELAY_TIMEOUT = 150ms` (`:8`) — a quick Alt-Tab tap switches with **no visible UI at all**.
  The mechanism is *not* "map the actor when the timer fires": `show()` (`:122-168`) takes the modal
  grab first (`:125`), maps at `opacity = 0` and forces an allocation (`:137-140`), makes the
  initial selection, and only then arms a timer whose body is `opacity = 255` (`:159-180`). The
  grab-before-delay ordering is load-bearing — the modifier release is delivered *to the grabbing
  popup*, so a grab deferred until the popup is visible would make the quick tap do nothing.
  Two things end the delay early: **any handled keypress** (`_showImmediately`, `:201-203`), so the
  rule is "150ms or the next Tab, whichever first"; and the **release race** (`:144-155`, bgo#596695)
  — the modifier may come up before the grab lands, so `show` samples pointer modifier state
  directly and commits on the spot if it is already up.
- `NO_MODS_TIMEOUT = 1500ms` (`:14`) — opened without a modifier held (e.g. from a gesture), it
  commits after this instead of on release (`:315-317`), and the deadline **re-arms on every key
  release** (`:229-231`).
- `DISABLE_HOVER_TIMEOUT = 500ms` (`:13`) — a **timer, not a motion latch**: `_disableHover`
  (`:290-303`) clears `mouseActive` and re-arms the timeout, which restores it when it fires;
  nothing waits for the mouse to travel. It re-arms on every keypress (`:198`) and scroll (`:244`),
  so held-down Tab keeps hover suppressed throughout.
- `POPUP_FADE_OUT_TIME = 100ms`, `POPUP_SCROLL_TIME = 100ms` (`:10-11`).
- Commit on modifier **release** (`:222-234`), Escape cancels, and `_finish` activates.

### Initial selection — forward starts at index 1

`_initialSelection` (`:113-120`): backward selects the last item, a one-item list selects 0, and
**everything else selects 1** — the *previous* window/app, not the current one. Start at 0 and
tap-and-release becomes a no-op, i.e. Alt-Tab appears broken. `backward` is `binding.is_reversed()`
(`windowManager.js:1705`), which is the `-backward` half of every switch binding — ten bindings the
table above omits (`windowManager.js:1673-1690`). `AppSwitcherPopup` overrides the whole thing for
`switch-group` (`altTab.js:118-137`): group-forward starts at (app 0, window 1) when the app has
more than one window.

### Keynav and commit inventory

Slice 1 claims "keynav rules" but never listed them, so there was nothing to implement against:

- **Destroy**: Escape or Tab, *if not consumed by the popup's own shortcut* (`:208-209`).
- **Commit**: Space, Return, KP_Enter, ISO_Enter (`:213-217`) — for no-modifier popups especially.
- **Move**: Left/Right with **RTL flipping** (`altTab.js:178, 193-206, 613-628`); Down enters the
  thumbnail sub-list, Up leaves it (`:197-198, 207-208`); wraparound is `mod()`
  (`switcherPopup.js:21-23, 182-188`). There is **no Home/End**.
- **Act on the selection**: `w`/`W`/`F4` closes the selected window (`altTab.js:199-200, 623-624`),
  `q`/`Q` quits the app (`:190-191`).
- Keys are matched by **keybinding action**, not keysym (`:196-197`,
  `global.display.get_keybinding_action`), so rebound switch keys keep working. We need an
  equivalent lookup or every one of these is hardcoded to a US layout.
- The `NO_MODS_TIMEOUT` deadline **re-arms on every key release** (`:229-231`) — it is not 1500ms
  from open.
- Commit is gated on `primaryModifier(mask)` — the *highest bit* of the binding's modifier mask,
  sampled from live pointer state, not tracked from key events (`:25-35, 223-227`).

### Live mutation, monitors, modality

- **Items disappear while the popup is up** — an app leaves RUNNING (`altTab.js:880-908`), a window
  is unmanaged (`:1082-1083, 1136-1145`) — with defined reselection rules
  (`switcherPopup.js:269-284`) and self-destruction when the list empties (`:282`). Reachable in one
  step: press F4 inside the switcher.
- **Everything is on the primary monitor**: popup placement (`:96, 104-109`), thumbnail placement
  (`altTab.js:88-114`), and the icon-ladder fit width (`:756-758`) all use
  `layoutManager.primaryMonitor`, never the focused one.
- **Inert in the overview**: the `switch-*`/`cycle-*` handlers are registered `ActionMode.NORMAL`
  only (`windowManager.js:691-731`). `system-modal-opened` destroys the popup
  (`switcherPopup.js:56-57`), `_startSwitcher` first kills any open workspace-switcher popup
  (`windowManager.js:1699-1701`), and becoming visible hides all OSDs (`switcherPopup.js:178`).
- **Pointer**: click-outside dismisses via a LongPressGesture (`:71-85`); clicking an item activates
  it, and clicking the already-selected *app* activates its current window (`:250-257`,
  `altTab.js:246-254`).

Our `mru.rs` has its own spellings of the first two (`open_delay_ms: 150`, `debounce_ms: 750`);
the 750 has no GNOME counterpart.

## Chrome (`data/theme/gnome-shell-sass/widgets/_switcher-popup.scss`)

- `.switcher-popup` — full-screen container, `padding: 0`, `spacing: $base_padding * 4` (24).
- `.switcher-list` — `%osd_panel` (the OSD's background/border, which `src/ui/osd.rs` already
  models), `padding: 12`, `border-radius: $modal_radius + 12`, plus a real drop shadow
  (`0 8px 8px 0`).
- `.switcher-list-item-container` — `spacing: $base_padding * 2` (12).
- `.item-box` — `tile_button` at OSD colours; **`:hover` is explicitly `background: none`** so the
  mouse cannot steal the highlight, and `:selected` is `transparentize($osd_fg_color, 0.8)`.
- `.thumbnail` — `width: 256px` (`THUMBNAIL_DEFAULT_SIZE`), `.thumbnail-box` padding 2 spacing 6.
- `.switcher-arrow` — the "this app has more than one window" chevron.
- `.cycler-highlight` — `border: 5px solid -st-accent-color`, the cyclers' whole UI.

`%osd_panel` is shared with the OSD, so the panel chrome is mostly a reuse of `src/ui/osd.rs`'s
colours — but the shadow and the `$modal_radius` are new.

## Item metrics (`altTab.js:13-30`)

- `APP_ICON_SIZE = 96`, ladder `baseIconSizes = [96, 64, 48, 32, 22]` — `AppSwitcher._setIconSize`
  (`:740+`) shrinks until the row fits the primary monitor, the same shape as the app grid's
  `_findBestIconSize` we already ported.
- `WINDOW_PREVIEW_SIZE = 128`, `APP_ICON_SIZE_SMALL = 48` — the Alt-Tab item is a 128 preview with
  the 48 app icon overlaid **bottom-right** (`x_align`/`y_align: END`, `WindowIcon:1049-1057`).

  Two details in `_createWindowClone` (`altTab.js:32-46`) that "a 128px preview" hides, and that
  our MRU switcher does not do:
  - `scale = Math.min(1.0, size / width, size / height)` — the leading **`1.0` clamp means a window
    smaller than 128px is drawn 1:1, never blown up**. A small dialog shows small.
  - the containing `_icon` is `set_size(size * scaleFactor, size * scaleFactor)` with the clone
    `CENTER`-aligned inside (`:1047`), so every item is the **same square** whatever the window's
    aspect. The row is uniform; the preview floats in its cell. (This is also what the 48px icon
    aligns `END` to — the 128 box, not the clone.) Note both the box and the clone size are
    multiplied by the stage scale factor, so 128 here is a logical size — ours must go through the
    usual logical→physical path, not be baked as pixels.
- `THUMBNAIL_POPUP_TIME = 500ms`, `THUMBNAIL_FADE_TIME = 100ms`, `APP_ICON_HOVER_TIMEOUT = 200ms`.

## The preview path (answers the slice-3 open question)

`src/ui/window_preview.rs` is **chrome only** — close button, caption, app-icon overlay, and the
hover-overlay gating. It has no window texture in it at all, so slice 3 cannot "reuse
`window_preview.rs`" as slice 3 originally said.

The live-texture-at-arbitrary-size path does exist, and it is not new: it is the sandwich

```
mapped.render_normal(..) -> clip (ClippedSurfaceRenderElement) -> RescaleRenderElement -> RelocateRenderElement
```

used today by `mru.rs` (`Thumbnail::render:441-465`), the overview's expose
(`layout/workspace.rs:2153`) and the workspace thumbnail strip (`layout/monitor.rs:2903`). It
composites the window's *live* surfaces at a scale — no offscreen, no snapshot — and the
rescale-rounding traps it walks into are already pinned in
`render_helpers/solid_color.rs::rescale_tests`.

So slice 3's real work is **extracting that sandwich** out of `mru.rs` before task 4 deletes it,
into a helper that fits a window into a fixed box (aspect-preserving, `1.0`-clamped, centered, per
`_createWindowClone` above). `mru.rs`'s copy already carries a `FIXME: deduplicate code with
Tile::render_inner()` at `:394` — the extraction is the answer to that FIXME too.

**One thing the extraction does not fix**, and slice 3 should decide on rather than inherit:
`mru.rs:460` reads `FIXME: this could use mipmaps, for that it should be rendered through an
offscreen.` Minification without mipmaps aliases, and the switcher is the **worst** case in the
codebase for it — a 1080p window into a 128px box is a ~1/8 downscale, where the overview's tiles
are nearer 1/3. Whether that is visible enough to need the offscreen is a *measurement*, not a
guess; take a shot of a text-heavy window in the switcher before deciding.

## Window list semantics (`getWindows`, `altTab.js:51-61`)

Not incidental: `Meta.TabList.NORMAL_ALL`, then **attached dialogs are mapped to their parent**,
then `skip_taskbar` windows and duplicates are dropped. Our MRU list has its own rules.

**This belongs to slice 1**, not to a later slice as an earlier draft left it unassigned: slice 1's
headline behaviour is a quick tap that commits without ever drawing, and committing means activating
an item from this list in this order.

### The gsettings — two schemas, and the defaults are *opposed*

An earlier draft of this doc said `app-switcher current-workspace-only` was "the only gsetting in
the whole subsystem". That is wrong, and the correction is user-visible
(`data/org.gnome.shell.gschema.xml.in:307-343`):

| schema | key | default | read by |
| --- | --- | --- | --- |
| `org.gnome.shell.app-switcher` | `current-workspace-only` | **false** | `AppSwitcher` |
| `org.gnome.shell.window-switcher` | `current-workspace-only` | **true** | `WindowSwitcherPopup._getWindowList` (`altTab.js:593-602`), `WindowCyclerPopup` (`:645-654`) |
| `org.gnome.shell.window-switcher` | `app-icon-mode` | `'both'` | `altTab.js:588`, item art (`:1026-1045`) |

So **stock Super-Tab spans workspaces and stock Alt-Tab does not.** Slice 3 must read the
window-switcher schema or it ships an Alt-Tab listing every workspace's windows — which looks
correct on a one-workspace test machine and wrong the moment anyone uses workspaces.

`app-icon-mode` also has two non-default layouts (`thumbnail-only`, `app-icon-only`) that change
what a `WindowIcon` contains; the item metrics above describe `both` only.

## Proposed slices

Each is independently landable and testable against the conformance corpus.

**Status 2026-07-31: slices 1–3 are landed, along with the `recent-windows`/`mru.rs` removal and
both `current-workspace-only` settings.** The multi-window arrow landed with them, on a real
triangle primitive (`Painter::triangle` / `sdf_triangle.frag`) rather than faked chrome — which is
also the verb slice 6's scroll arrows will want. Slices 4–6 and `Above_Tab`'s per-layout resolution
are untouched.

**Slice 5 (the window sub-list) landed 2026-08-01**, together with the popup's own key routing
(the arrows, Escape, the explicit-commit keys, `w`/`F4` and `q`), the 100 ms fade, and the
preview corner rounding. The one thing deliberately **not** ported is `APP_ICON_HOVER_TIMEOUT`'s
200 ms delay before a hovered app swaps its sub-list: it only matters once a sub-list is already
up, and our hover is already gated on `mouseActive` (off for 500 ms after any keypress), so it
would be a third timer interacting with two others for a case we have not seen misbehave. Port it
against a real complaint.

**Slice 4 (`switch-group`) landed 2026-08-01.** `switch-group` / `switch-group-backward` are now
adopted from `org.gnome.desktop.wm.keybindings` with GNOME's own defaults
(`<Super>Above_Tab` / `<Alt>Above_Tab`, plus the `<Shift>` twins), and `Above_Tab` parses to the
`KEY_GRAVE + 8` **keycode** — see the correction at the top of this doc; there is no per-layout
resolution left to do. It opens the same `AppSwitcherPopup` on the same full app list — you can
still tab out of the group — differing only in `_initialSelection`: app 0 (the app you are in)
with its sub-list already up, on window 1 forward and the last window backward
(`altTab.js:118-137`). While it is up, a repeat press walks that app's windows rather than the app
row (`_keyPressHandler`, `altTab.js:180-186`). Slice 6 is what remains.

**Slice 6 (the cyclers) landed 2026-08-01, closing the port.** `cycle-windows` (`<Alt>Escape`)
and `cycle-group` (`<Alt>F6`), with their `<Shift>` twins, are adopted from
`org.gnome.desktop.wm.keybindings`. `CyclerPopup` (`altTab.js:487-540`) is the same session
machinery with **no list**: `CyclerList` (`:472-484`) draws nothing, so it lands as an
`Items::Cycler` variant of the existing surface rather than a second popup type — the grab,
the timers, the modifier-release commit and Escape are all the code that was already there.

Three things are specific to it:

- **It shows immediately.** `_highlightItem` runs from `_initialSelection` inside `show()`, and
  `POPUP_DELAY` only ever set the popup actor's opacity — an actor a cycler does not have. So the
  highlight is up on the first press, not 150 ms later.
- **The raise.** GNOME clones the window into `global.window_group`, raises the clone and hides
  the original (`CyclerHighlight`, `:410-472`). We own the render loop, so `FloatingSpace::render`
  simply draws that one tile first and skips it in place — cheaper, and free of the
  double-composite a clone over a still-visible original would give a translucent window. Nothing
  in the stacking order changes, so Escape leaves the stack as it found it.
- **The frame** is `.cycler-highlight`: `border: 5px solid -st-accent-color`
  (`_switcher-popup.scss:80-82`), which St draws *inside* the widget's box. That is a new toolkit
  verb — `render_helpers::inset_ring::InsetRing` — because `FocusRing` is the *outset* sibling and
  a single `BorderRenderElement` fills its whole area, which would paint over the window the frame
  exists to show.

**A key routing fix rode along.** The switcher-open binding allowlist was one "is it open" flag;
it is now `SwitcherGrab`, which knows *which* popup is up. That matters because a popup consumes a
key only when it resolves to an action its own `_keyPressHandler` matches, and everything else
falls through to the base's Escape / Tab / commit handling — which is precisely how `<Alt>Escape`
abandons an `<Alt>F6` cycler instead of driving it (`switcherPopup.js:206-210` says so out loud).

**Known divergence left standing:** the *popup* arm of that allowlist still lets all three switch
actions resolve while any list popup is up. GNOME does not — `AppSwitcherPopup` ignores
`SWITCH_WINDOWS` and `WindowSwitcherPopup` ignores `SWITCH_APPLICATIONS`, so the keysym reaches
the base and Tab **destroys** the popup. Ours advances it instead. Left alone deliberately: it is
pre-existing, it is the friendlier behaviour, and changing it touches the two slices most
exercised on the seat.

**Renderer gap found on the way (not switcher-specific):** `Frame::draw_solid` does not consult
the rounded clip an outer `ClippedSurfaceRenderElement` armed, where `render_texture_from_to`
does. So a surface backed by a *single-colour* buffer keeps square corners wherever a rounded
clip applies — invisible for ordinary clients, but it made the sub-list's rounding look broken
against a test fixture using `attach_solid_buffer`. The fix is a clipped-solid path mirroring
`render_clipped_texture`.

**Seat validation 2026-08-01 found two Alt-Tab divergences, both fixed.** (a) The app badge was
pushed *behind* the window preview, so a square window buried half of it — `WindowIcon` adds the
clone and then the icon to one `Clutter.BinLayout` (`altTab.js:1029-1037`), so the icon is the
later child and paints on top. (b) Window titles were drawn per item, where a title longer than
the 128 px preview overflowed its slot. They are not per-item in GNOME: `WindowIcon`'s label is
only handed to `addItem` as the accessible `label_actor` (`switcherPopup.js:451-464`), and
`WindowSwitcher` owns **one** `St.Label` across the whole list (`altTab.js:1066-1070`) whose text
follows the selection (`highlight`, `:1130-1134`), ellipsized like any `StLabel`. Both are the
same lesson as the arrow: read where the JS *adds the child*, not where the pixels look right.

1. **Shared base + chrome.** `SwitcherPopup` timing/keynav/commit rules, the window list
   (`getWindows` semantics, below) that `_finish` needs to activate anything, and the
   `.switcher-list` panel. No item art. **Does not touch the `switch-*` bindings** — see the
   handoff note below.
2. **`AppSwitcherPopup`** (`<Super>Tab`): app grouping via `app_system`, the 96px icon ladder, the
   multi-window arrow, `current-workspace-only`.
3. **`WindowSwitcherPopup`** (`<Alt>Tab`): 128px window previews with the 48px app icon overlay and
   the title label. Extracts the live-preview sandwich out of `mru.rs` first — see "The preview
   path".
4. **`switch-group`** (`Above_Tab`): the within-app entry into the app switcher.
5. **Thumbnail sub-list**: the down-arrow / 500ms-hover window strip under a selected app, 256px
   thumbnails.
6. **Cyclers** (`<Alt>Escape`, `<Alt>F6`): no list, just `.cycler-highlight` — a 5px accent border
   drawn around each window in turn.

Slices 1–3 are the ones a user would call "Alt-Tab works like GNOME". 4–6 are completeness.

**Agreed 2026-07-31:** do 1–3 now; remove niri's `recent-windows` config block and `mru.rs`;
resolve `Above_Tab` per keyboard layout like mutter, not hardcoded backtick.

**Sequencing amendment.** The agreement above put the `recent-windows` removal *in slice 1*. That
is wrong on inspection, for two reasons that point the same way:

- slice 1 deliberately has no item art, so deleting `mru.rs` there lands a commit where every
  `switch-*` binding opens an empty panel;
- `mru.rs` is the donor for the preview sandwich slice 3 has to extract, and
  `synoik-config/src/recent_windows.rs` is what *parameterizes* `mru.rs` — so the config block cannot
  go before `mru.rs` either without stranding it on hardcoded defaults.

Both moves go **after slice 3**, together, when both replacements exist.

**Binding handoff.** The amendment above relocates the broken middle rather than removing it unless
ownership of the `switch-*` actions is stated, so: **`mru.rs` keeps both bindings until its
replacement lands.** Slice 1 wires nothing; slice 2 flips `switch-applications` to
`AppSwitcherPopup`; slice 3 flips `switch-windows` to `WindowSwitcherPopup`; the removal follows.
Every commit therefore leaves both bindings working.

That leaves slice 1 with no keybinding to drive it, which the conformance convention
(drive `State::do_action`) would normally reject. Slice 1 is testable anyway *because* the base is
list-agnostic: drive it with a synthetic item list through the same entry point the popups will use.
If that seam turns out not to exist, slice 1 should merge into slice 2 rather than grow a test-only
door — [[test-the-code-not-a-reimplementation]].

## Open questions

- ~~**Window previews at 128px**~~ — **ANSWERED 2026-07-31, see "The preview path" below.**
- ~~**`Above_Tab` resolution**~~ — **CLOSED 2026-08-01 on a false premise.** The doc claimed mutter
  resolves it per layout; it does not (see the correction at the top). We match `KEY_GRAVE + 8` by
  keycode, exactly as mutter does, and that is already layout-independent.
- ~~**The cyclers** draw on top of the windows themselves~~ — **ANSWERED 2026-08-01.** They go out
  at the top of the window layer (`CyclerHighlight` is a child of `global.window_group`,
  `altTab.js:498`), so they stay under the panel and the layer-shell surfaces above it. The
  "raise" half is a draw-order skip in `FloatingSpace::render`, not a restack.

## Amendment 2026-08-09: what a switcher *shows* you, and what its commit does

Three changes, landed together because they are one idea: the switcher should show you the state
its commit would produce, and its commit should produce the state GNOME's does.

### 1. Committing on an app raises the whole app — conformance, not divergence

`shell_app_activate_window` (`shell-app.c:381-442`) brings **every** window of the app forward, not
just the one it focuses. We only ever raised the focused one, which is invisible in the common
one-window case and is why it went unnoticed for the whole port. Three details come straight from
there and are all load-bearing:

- only windows on the **target's workspace** are raised (`:413-415`) — activating an app must not
  reshuffle its windows on desktops you are not looking at;
- they are raised **in reverse order** (`:416-425`), so the group arrives with its own relative
  stacking intact rather than re-sorted by MRU;
- only the *app row's* commit does this. A picked thumbnail is `Main.activateWindow`
  (`altTab.js:285`), one window and no group raise — as are the window switcher (`:632`) and the
  cyclers (`:525`).

This lands as `Layout::raise_under`, on top of a new `FloatingSpace::raise_window_only` (a raise
that does not touch the active window, i.e. `meta_window_raise_and_make_recent_on_workspace`).
The switcher hands the compositor an `Activation` — the windows to bring forward, topmost first —
instead of the single `Option<MappedId>` it used to, and that one list is also what the preview
below reads, so the two cannot drift.

Fixed on the way past: `SwitcherUi::open` returned the outcome *without* a target, so a session
that lost the release race at open (bgo#596695, `switcherPopup.js:144-155`) committed to nothing.

### 2. DIVERGENCE: `switch-group` is a window switcher, not a pinned app switcher

GNOME opens `AppSwitcherPopup` on (app 0, window 1) with the sub-list already up
(`altTab.js:117-137`), spending the top of the popup on an app row you cannot usefully move in.
Every item in that session belongs to one app by construction, so we show that app's windows
directly: `Items::Windows` over `focus_app.get_windows()`, same previews, same footer title, no
row and no sub-list. Its own binding pressed again simply walks the list. Commit stays
`Main.activateWindow` — one window, no group raise — which keeps Alt-Tab and Above_Tab consistent
with each other and leaves the group raise as the thing Super-Tab does.

The `group` flag on `OpenRequest` and the pinned initial selection it drove are gone with it.

Side effect, intended: because the rewrite matches on `Items::Windows`, pressing Above_Tab while
an *Alt-Tab* popup is up now advances that list too, where GNOME ignores it there. Both popups are
window lists at that point and the key means "next window" in one of them, so making it mean
nothing in the other is a distinction without a difference.

### 3. DIVERGENCE: every switcher previews what it would raise

GNOME previews nothing outside the cycler. The cycler's raise — a draw-order skip in
`FloatingSpace::render`, see the slice 6 note above — generalizes to all three switchers and to a
whole app's worth of windows: `cycler_raised: Option<W::Id>` becomes `preview_raised: Vec<W::Id>`,
topmost first, fed from the same `Activation` the commit uses.

Rules, and why each is the way it is:

- **Draw order only.** No restack, no focus change, nothing a client can observe. A preview that
  really raised would be indistinguishable while the popup is up and would silently destroy
  Escape's guarantee that the stack is left exactly as it was found. The corpus asserts both
  halves for that reason.
- **Gated on visibility** for the popups: a tap too quick to show a popup must not shuffle the
  screen on its way through either. The cycler is exempt — raising the window *is* its whole UI,
  and it starts from `_initialSelection` inside `show()`.
- **Workspace-restricted**, same rule as the commit's raise: the preview is a rehearsal, so it
  must not promise a stacking the commit will not perform.

**The workspace half.** When the selection lives on another workspace, that monitor animates to it
— a real `switch_workspace`, so it is the real animation. Abandoning the session animates back;
committing keeps it. Two things make it safe to do for real:

- **A dwell** (`WORKSPACE_PREVIEW_DELAY`, 150 ms, ours). The raise half is instant because it costs
  a draw order; a workspace slide is not, and tabbing across apps on three workspaces would start,
  interrupt and reverse one per keypress. Re-armed from scratch on every move, so holding Tab down
  never settles anywhere — a dwell, not a rate limit.
- **The `previous_workspace_id` bookmark is preserved.** A workspace you previewed and abandoned is
  not somewhere you "were": "switch to previous workspace" must not learn about workspaces a
  switcher merely passed through. On commit the bookmark is pointed at where the *session* started,
  not at the last stop before it.

Origins are recorded per output on first touch (`Synoik::switcher_ws_preview`), so a monitor
tabbed across three workspaces is still owed back the one the session started on. A session that
ends without reaching `finish_switcher` — a modal or the lock screen taking the grab — is handed
back by the per-frame `sync_switcher_preview`, which owns both halves of the preview and is the
single writer for them.

The workspace restore resolves its origin **by `WorkspaceId`**, falling back to the recorded index
only if that workspace is gone: `w`/`F4` closes windows without ending the session, so the strip
can renumber under a live preview.

Corpus: `super_tab_brings_the_whole_app_forward_not_just_its_focused_window`,
`switch_group_walks_the_current_apps_windows_as_a_window_switcher`,
`a_switcher_shows_what_it_would_raise_and_puts_it_back_on_escape`,
`the_switcher_previews_another_workspace_and_gives_it_back`.
