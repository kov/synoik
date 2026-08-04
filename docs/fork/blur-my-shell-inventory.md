# Blur my Shell — behavior inventory (v72, read 2026-08-04)

Source read: `~/.local/share/gnome-shell/extensions/blur-my-shell@aunetx` (v72), plus Gustavo's
own settings (`dconf dump /org/gnome/shell/extensions/blur-my-shell/`). This is an inventory to
decide from, not a plan — what has been adopted so far is the two items in `overview-port.md` §13
plus §1's plates (see §5).

## 0. What Gustavo actually runs

Everything is at its default except:

| key | his value | default |
| --- | --- | --- |
| `panel/unblur-in-overview` | `false` | `true` |
| `dash-to-dock/*` | blur on, static, sigma 30, brightness 0.6 | (same as default) |
| `*/brightness` | `0.6` (written explicitly) | `0.6` |
| `*/sigma` | `30` (written explicitly) | `30` |

`unblur-in-overview=false` is the interesting one: upstream hides the panel blur while the overview
is up; he turned that off, so on his session **the panel stays blurred and dimmed in the overview**
rather than going flat. Ours currently does neither — it keeps the blur but fades the dark wash to
nothing, because GNOME's `#panel:overview` drops the background. See §2.2.

The `dash-to-dock` component only ever attaches to an actor named `dashtodockContainer` of class
`DashToDock` (`components/dash_to_dock.js:229-234`) — the Dash to Dock *extension*. He does not run
it, so those keys do nothing.

## 1. The finding: the translucent dash is not a dash blur — **adopted 2026-08-04, see §5**

**It is a stylesheet swap over the whole overview**, `overview/style-components` (default `1` =
"light"), which adds one class to `Main.uiGroup` (`components/overview.js:163-172`) and lets
`stylesheet.css` restyle everything under it. In the default "light" variant:

- `#dash > .dash-background` → `rgba(200,200,200,0.2)` (this is the translucent dash)
- `.search-entry` → `rgba(200,200,200,0.2)`, white text, no border, no shadow
- `.workspace-thumbnail` → `rgba(200,200,200,0.2)`
- `.overview-tile`, `.overview-icon`, `.grid-search-result`, `.list-search-result`,
  `.search-provider-icon` → fully transparent (the app-grid and search tiles lose their plates)
- `.overview-tile.app-folder`, `.page-navigation-arrow`, `.search-section-content`,
  `.app-folder-dialog .icon-button` → `rgba(200,200,200,0.2)`
- every hover/focus/active/selected state re-specified as `rgba(230,230,230,0.08…0.3)`

Three variants exist — `light` (default), `dark` (`rgba(100,100,100,0.35)`), `transparent`
(alpha 0, tiles keep only their hover states). So "make the dash translucent" is really "restyle
the overview chrome for a blurred backdrop", and adopting it piecemeal (dash only) would leave the
search entry and tiles looking heavier than the dash.

For us this is a `widget::`/theme-node change, not a renderer one: our equivalents are the dash
plate, `overview_search`'s entry pill, `thumbnail_chrome`, `app_grid`'s tiles and
`overview_search`'s result cards. `docs/fork/gnome-style-reference.md` holds what those are now.

## 2. Per-surface inventory

Default = the extension's own default, not Gustavo's. "Ours" = synoik today.

### 2.1 Overview backdrop — default ON — **we have this**
Blurred wallpaper behind the overview, sigma 30 / brightness 0.6, via a `Meta.BackgroundGroup`
inserted at index 0 of `overviewGroup` and pinned there on `child-added`. Landed for us at
brightness 0.45 (`overview-port.md` §13).

### 2.2 Panel — default ON — **we have the blur; four sub-behaviors we do not**
- `static-blur` (default **true**): blur the *wallpaper* under the panel, from its own background
  actor, rather than the live scene. Dynamic mode uses `Shell.BlurEffect` on the panel actor and
  needs a repaint HACK for shadows (`components/panel.js:207-215`). **Ours is dynamic** — it
  captures the real framebuffer, so a window scrolled under the bar shows through. Strictly more
  general; the visible difference only appears when something other than wallpaper is behind.
- `override-background` (default true) + `style-panel` (default 0 = `transparent-panel`): the CSS
  that makes the bar see-through. Variants: transparent / light `rgba(200,200,200,0.2)` / dark
  `rgba(100,100,100,0.35)` / **contrasted** — transparent bar, but every `.panel-button` and the
  clock get their own `rgba(0,0,0,0.8)` rounded plate. Ours is a flat black wash at α0.4, i.e.
  between "dark" and its own thing.
- `unblur-in-overview` (default true, **Gustavo: false**): §0.
- `override-background-dynamically` (default false): make the panel opaque again whenever a window
  comes within `5 × scale` px of it, per monitor, recomputed on window add/remove, workspace
  switch, overview show/hide (`components/panel.js:453-514`). This is the "panel is only
  translucent over the desktop" behavior.
- `force-light-text` (default false): a `panel-light-text` class forcing `#f6f5f4` foreground and
  re-specifying every panel-button state, for a light wallpaper under a transparent bar. Ours has
  the same problem — it is why the overview backdrop went to 0.45.

### 2.3 App folder dialog — default ON — **we do not have this**
Blurs behind the open app-folder dialog, and `appfolder-dialog-*` CSS makes the dialog itself and
its `.folder-name-entry` transparent/light/dark (`style-dialogs`, default 1 = light). It also
patches `_zoomAndFadeIn`/`_zoomAndFadeOut` to animate the blur with the dialog
(`components/appfolders.js:148-182`). We have folder dialogs (`ui/folder_dialog.rs`).

### 2.4 Screenshot window selector — default ON — **we do not have this**
Blurs the backdrop of the screenshot UI's window-picker mode
(`Main.screenshotUI._windowSelectors`). We have that surface (`ui/screenshot_ui.rs`, window mode
fills `$system_base_color`).

### 2.5 Lock screen — default ON — **GNOME already does this; we implement GNOME's**
GNOME natively blurs the lock wallpaper (`unlockDialog.js`, radius 90 / brightness 0.65), which is
what `ui/lock_screen.rs` implements. The component re-does it through its own pipeline so the
user's sigma/brightness apply. Nothing to adopt unless we want the knob.

### 2.6 Applications (per-window) — default **OFF** — **partly ours already**
Two separate things bundled:
- blur *behind* a translucent window, whitelist/blacklist by `wm_class` with wildcards. This is
  what our `ext-background-effect-v1` + xray path already does, client-driven rather than
  compositor-forced.
- `opacity` (default 215/255) **forced onto the window actor**, with `dynamic-opacity` (default
  true) making the *focused* window solid and unfocused ones translucent. That is a real behavior
  we do not have and could not get from the protocol — it is the compositor overriding the client.
  Note our own history here: [[ghost-translucency-stale]] — forced window translucency was removed.
- `blur-on-overview` (default false) and a workspace-switch hook that un-hides neighbouring
  workspaces' windows so their blur shows during the switch.

### 2.7 Workspace-switch animation — part of the overview component — **we do not have this**
A second blurred background group is inserted above `global.window_group` for the duration of a
workspace switch, and `.workspace-animation { background-color: transparent }` so the blurred
wallpaper shows in the gap between workspaces instead of black
(`components/overview.js:65-116`). Our GNOME-mode switch is a slide between full-bleed
workspaces, so the equivalent question is what shows in the gap.

### 2.8 Window list / Coverflow Alt-Tab / Dash to Dock / hidetopbar / dash-to-panel
Compatibility shims for other extensions. Not applicable.

## 3. The effect vocabulary (if we ever want knobs)

Blur my Shell composes **pipelines** — named, ordered lists of effects, stored as one
`a{sa{sv}}` gsetting, with each surface naming a pipeline. Effects available: `gaussian_blur`,
`native_static_gaussian_blur`, `native_dynamic_gaussian_blur`, `monte_carlo_blur`, `pixelize`,
`color` (tint), `corner` (rounding), `noise`, `luminosity`, plus `downscale`/`upscale`/`derivative`
as chain helpers. The two shipped defaults are `pipeline_default` (native static gaussian, radius
30, brightness 0.6) and `pipeline_default_rounded` (the same plus a `corner` pass).

We have no config file, so a pipeline system is not the shape to copy. It is listed because it
explains why every surface has `sigma`/`brightness`/`color`/`noise-*` keys: they are the *legacy*
per-surface knobs, kept working by a `customize` flag that switches between "use my pipeline" and
"use these five values".

## 4. Suggested order, if we adopt more

1. **Overview chrome translucency** (§1) — the one Gustavo noticed, and the one that makes the
   backdrop blur we just landed look finished rather than half-applied. Biggest visible win.
2. **Panel dynamic override** (§2.2) — panel opaque when a window is near it. Off by default
   upstream, but it is the honest answer to "white text over a light wallpaper" and would let the
   bar be *more* transparent over the desktop than 0.4.
3. **App folder dialog + screenshot selector blur** (§2.3, §2.4) — small, self-contained, and both
   surfaces already exist here.
4. **Workspace-switch gap** (§2.7) — depends on what our switch actually shows today; check before
   costing.
5. **Forced window opacity** (§2.6) — deliberately last. It overrides the client, we removed
   something like it before, and the protocol-driven path we already have covers the honest case.

## 5. What we took from §1 (landed 2026-08-04)

Adopted: **the plates**. One shared `ui::widget::style::OVERVIEW_PLATE`
(`rgba(200,200,200,.2)` — the "light" variant's value) now fills the four surfaces the overview
lays over its backdrop: the dash pill (`DASH_BG`), the search entry (`EntryStyle::Search`), the
search-results card (`.search-section-content`) and the app grid's folder tiles (`FOLDER_BG`, the
grid's only tile with a resting fill). One constant, so they cannot drift — they read as one
material only if they are one colour.

**Not adopted: its re-specification of every interaction state** at `rgba(230,230,230,.08–.3)`.
Ours are relative washes over whatever they sit on (`HOVER_WASH`, and an accent-derived focus
fill), so they already compose over a translucent plate — and keeping them keeps GNOME's accent in
the focus state, which that stylesheet drops. The one absolute state colour that had to go was the
dash's `TILE_HOVER` (`st-lighten($dash_background_color, 7%)`), an opaque value derived from the
opaque pill; it is `HOVER_WASH` now, which is what the app grid already used for the same gesture.

Two things fell out of it worth knowing:

- `.overview-tile` needed no change: our app-grid tiles are already transparent at rest, which is
  what that stylesheet's rule asks for.
- `style::over` was pinning its result's alpha at 1, correct only while every surface it served was
  opaque. The dash's separator uses it, because `Painter::hairline` *clears* rather than blends —
  so on a translucent plate it drew a solid bar across the one thing the backdrop is meant to show
  through. It is a real source-over now (identical for an opaque base).

Left from §1 for whoever picks it up: `.workspace-thumbnail`'s plate (ours draws the wallpaper, so
there is nothing to see through), `.page-navigation-arrow`, and `.app-folder-dialog .icon-button` —
that last one belongs with §2.3's dialog blur, not here.
