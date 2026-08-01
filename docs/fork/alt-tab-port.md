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

`Above_Tab` is the key above Tab — backtick on most layouts, and *not* hardcoded: mutter resolves
it per layout.

## What we have

`src/ui/mru.rs` — niri's most-recently-used window switcher: window previews in a row, a border on
the selected one, a title underneath. One popup for every binding above, differing only by a
`MruFilter` (`All` / `AppId`) and an `MruScope` (`All` / `Output` / `Workspace`).

The **keybindings are already re-homed** onto GNOME's schema (`src/gnome.rs:863+`
`adopted_wm_keybindings`), and `GnomeKeyAction::SwitchApplications` already carries the note that
this port exists to retire:

> GNOME groups by application and spans workspaces; we map it onto the window MRU switcher over
> all workspaces (no app grouping — accepted divergence for now).

So this is not a new divergence to negotiate — it is a debt already written down.

niri's own config surface (`niri-config/src/recent_windows.rs`: `recent-windows` with
`debounce-ms`, `open-delay-ms`, `highlight`, `previews`, `binds`) is "niri's way" for a thing GNOME
does differently, so per the fork tenet it goes. GNOME's equivalents are fixed constants plus one
gsetting (below).

## The shared base — `SwitcherPopup` (`switcherPopup.js`)

Everything below sits on this, and it is where the *feel* lives:

- `POPUP_DELAY_TIMEOUT = 150ms` (`:8`) — the popup does not appear until the modifier has been
  held this long; a quick Alt-Tab tap switches with **no visible UI at all** (`:152-165`).
- `NO_MODS_TIMEOUT = 1500ms` (`:14`) — opened without a modifier held (e.g. from a gesture), it
  commits after this instead of on release (`:315-317`).
- `DISABLE_HOVER_TIMEOUT = 500ms` (`:13`) — after a keypress, pointer motion is ignored until the
  mouse actually moves, so a stationary pointer under the popup cannot steal the selection
  (`:263-296`).
- `POPUP_FADE_OUT_TIME = 100ms`, `POPUP_SCROLL_TIME = 100ms` (`:10-11`).
- Commit on modifier **release** (`:217-228`), Escape cancels, and `_finish` activates.

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
- `THUMBNAIL_POPUP_TIME = 500ms`, `THUMBNAIL_FADE_TIME = 100ms`, `APP_ICON_HOVER_TIMEOUT = 200ms`.

## Window list semantics (`getWindows`, `altTab.js:50-59`)

Not incidental: `Meta.TabList.NORMAL_ALL`, then **attached dialogs are mapped to their parent**,
then `skip_taskbar` windows and duplicates are dropped. Our MRU list has its own rules.

`org.gnome.shell.app-switcher current-workspace-only` (default **false**) decides whether the app
switcher spans workspaces — the only gsetting in the whole subsystem.

## Proposed slices

Each is independently landable and testable against the conformance corpus.

1. **Shared base + chrome.** `SwitcherPopup` timing/keynav/commit rules and the `.switcher-list`
   panel, driven by the existing `switch-*` `GnomeKeyAction`s. Retires niri's `recent-windows`
   config block. No item art yet — this is the slice that makes a quick tap show nothing.
2. **`AppSwitcherPopup`** (`<Super>Tab`): app grouping via `app_system`, the 96px icon ladder, the
   multi-window arrow, `current-workspace-only`.
3. **`WindowSwitcherPopup`** (`<Alt>Tab`): 128px window previews with the 48px app icon overlay and
   the title label. Reuses whatever `src/ui/window_preview.rs` can give us.
4. **`switch-group`** (`Above_Tab`): the within-app entry into the app switcher.
5. **Thumbnail sub-list**: the down-arrow / 500ms-hover window strip under a selected app, 256px
   thumbnails.
6. **Cyclers** (`<Alt>Escape`, `<Alt>F6`): no list, just `.cycler-highlight` — a 5px accent border
   drawn around each window in turn.

Slices 1–3 are the ones a user would call "Alt-Tab works like GNOME". 4–6 are completeness.

**Agreed 2026-07-31:** do 1–3 now; remove niri's `recent-windows` config block as part of slice 1;
resolve `Above_Tab` per keyboard layout like mutter, not hardcoded backtick.

## Open questions

- **Window previews at 128px**: `window_preview.rs` was built for the overview. Does it give us a
  live-texture-at-arbitrary-size path, or does the switcher need its own?
- **`Above_Tab` resolution**: mutter resolves "the key above Tab" per layout — agreed we do the
  same. Hardcoding backtick is correct on US layouts and silently wrong elsewhere, which is exactly
  the kind of thing this machine cannot show us.
- **The cyclers** draw *on top of the windows themselves*, not in a panel — a different render path
  from everything in slices 1–5.
