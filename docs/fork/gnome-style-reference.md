# GNOME Shell 50.1 style reference

**Purpose.** One place that answers "how should this look?" for every gnome-shell / mutter
component we port, grounded in the actual **GNOME 50.1** source (`~/Projects/gnome-shell`,
`~/Projects/mutter`) — never memory. Each component links its CSS class(es) to concrete
render values (font pt/weight, colors, radius, padding, icon size) and to **our** implementation
(the Rust constant + a status flag). We re-derived the same handful of tokens three times porting
the quick-settings menu alone; this table is so we stop.

**Reference-first rule (from CLAUDE.md).** Before porting/changing any behavior or layout, read the
50.1 source and cite the file. This doc is a cache of that reading, not a substitute — if a row
looks wrong, re-grep the reference and fix the row.

**Positioning & child order are not in these tables.** They come from the JS construction order
(`js/ui/quickSettings.js` `_addItems`/`addItem`, `js/ui/panel.js` box assembly, `add_child` calls),
not the SCSS — the CSS says how a widget *looks*, never *where* it sits. Before placing a widget in
a container, read that sequence and cite it, the same way you'd cite a color. (This is exactly the
trap that put the volume slider at the bottom of the QS menu instead of between the system row and
the toggle grid — the style was reference-checked, the order was assumed.)

**How to read the tables.** Values are resolved to concrete units (px, pt, "fully rounded"). GNOME
authors in `pt` for fonts and `$token` multiples for space; the token legend below resolves them.
The `Source` column is `path:line` into the reference checkout, e.g. `widgets/_panel.scss:12` under
`data/theme/gnome-shell-sass/`, or `js/ui/…` / `src/st/…`.

**Where our status lives.** The §2 reference tables are pure **GNOME spec** — status per element for
hundreds of not-yet-built rows would be noise. Our port state is tracked at component granularity in
the **§3 port-status ledger** (module, status, key constants, divergences), and the two shipped
components (panel, quick-settings) get an element→constant mapping there. Status flags:
- ✅ done — matches GNOME (or an accepted divergence, noted)
- 🟡 partial — implemented but incomplete / not yet pixel-matched
- ⬜ TODO — not yet ported
- ⚠️ diverged — intentionally different from GNOME (reason noted)

---

## 1. Design tokens (the shared foundation)

Everything below is reused by every widget. Source: `data/theme/gnome-shell-sass/_common.scss`,
`_default-colors.scss`, `_palette.scss`, `_drawing.scss`, and `src/st/st-theme-context.c`.

### 1.1 Spacing, radii, icon sizes

| Token | Value | Notes | Source |
|---|---|---|---|
| `$base_font_size` | 11pt | base UI font | `_common.scss:30` |
| `$base_padding` | 6px | internal padding unit; most paddings are multiples of this | `_common.scss:31` |
| `$base_margin` | 4px | inter-element margin | `_common.scss:32` |
| `$base_border_radius` | 8px | radius on most elements | `_common.scss:33` |
| `$modal_radius` | 16px | `= base_border_radius * 2`; dialogs, menus, QS panel base | `_common.scss:40` |
| `$alert_radius` | 18px | alert/notification banners | `_common.scss:43` |
| `$forced_circular_radius` | 999px | "fully rounded" — pills & circles | `_common.scss:37` |
| `$base_icon_size` | 16px | symbolic icon | `_common.scss:51` |
| `$medium_icon_size` | 24px | `*1.5` | `_common.scss:52` |
| `$large_icon_size` | 32px | `*2` | `_common.scss:53` |
| `$scalable_icon_size` | 16px (em) | the em-relative icon size used inside buttons/toggles | `_common.scss:60` |
| `$scaled_padding` | 6px (em) | em-relative `$base_padding` | `_common.scss:57` |
| `$ease_out_quad` | cubic-bezier(.25,.46,.45,.94) | default easing | `_common.scss:65` |

### 1.2 Font scale (the `%`-placeholders)

GNOME sizes text through a handful of placeholder classes, not per-widget px. Each is `pt` +
`font-weight`. Source `_common.scss:241-287`.

| Placeholder | pt | weight | 96-DPI px¹ | used for |
|---|---|---|---|---|
| `%large_title` | 24 | 300 | 32.0 | clock on lock screen |
| `%title_1` | 20 | 800 | 26.7 | big headings |
| `%title_2` | 15 | 800 | 20.0 | headings |
| `%title_3` | 15 | 700 | 20.0 | dialog titles |
| `%title_4` | 13 | 700 | 17.3 | sub-headings, bt placeholder |
| `%heading` | 11 | 700 | 14.7 | **quick-toggle titles, list titles** |
| `%caption_heading` | 9 | 700 | 12.0 | small bold captions |
| `%caption` | 9 | 400 | 12.0 | subtitles, secondary text |
| body (default) | 11 | 400 | 14.7 | panel labels, menu items |
| `%numeric` | — | — | — | `font-feature-settings: "tnum"` (tabular figures; the clock) |

¹ **pt→px is a single factor now** — `PX_PER_PT = 4/3` via `ui::pt_to_px` (see §1.3). GNOME/St
converts the base `pt` at the stage's font DPI (nominally 96 DPI → `1pt = 4/3 px`, the "96-DPI px"
column); the theme's `fontsize` mixin `1.091` is only an internal em-ratio, not the realized factor.
Validated against a real Cantarell 50.1 panel at matched scale: every text element within ~10%.

### 1.3 pt→px — one helper (`ui::pt_to_px`)

**Resolved.** Font sizes now go through `ui::pt_to_px(pt)` over a single `PX_PER_PT` constant
(GNOME's nominal 96 DPI, `4/3`), so every UI is expressed as its **GNOME point size** and scales
together. Before this, our px drifted: 11pt was realized as 13px in the panel/QS but 12–14px in the
dialogs/MRU/hotkey overlay (~10–25% under GNOME). To size text, look up the widget's `%`-placeholder
pt in §1.2 and call `pt_to_px`.

| UI | our const | GNOME size → px (`4/3`) |
|---|---|---|
| panel clock (bold) | `panel::FONT_PX` | 11pt → 14.7 |
| QS tile title / battery % (bold) | `quick_settings::LABEL_PX` | 11pt `%heading` → 14.7 |
| end-session title / body | `end_session_dialog::TITLE_PX` / `BODY_PX` | 15pt `%title_3` → 20.0 / 11pt → 14.7 |
| run-dialog base / small | `run_dialog::BASE/SMALL_FONT_PX` | 11pt → 14.7 / 9pt `%caption` → 12.0 |
| exit-confirm / MRU / hotkey / config-error | `…::FONT_PX` | 11pt → 14.7 |
| calendar month / weekday+day | `calendar::HEADER_PX` / `WEEKDAY_PX`,`DAY_PX` | 11pt → 14.7 / 9pt → 12.0 |

**The point sizes in this document are nominal, not absolute.** GNOME's `fontsize($size)` mixin
(`_drawing.scss:69-75`) reduces to `font-size: ($size/11pt)em` — the `1.091` and the `16px` inside it
cancel — so the theme never states a size in points, only a *ratio* against the stage. The stage is
`fontsize($base_font_size)` = `1em`, which St resolves against the theme context's font: built from
`StSettings:font-name` (`st-theme-context.c:240-243`) = `org.gnome.desktop.interface font-name`
(`st-settings.c:32,509`), and re-derived when it changes (`on_font_name_changed`, `:339-344`).

So on a desktop set to "Cantarell 12" every GNOME string renders 12/11 larger than the pt this table
quotes. `ui::pt_to_px` scales by that ratio (`ui::base_font_pt`, fed from the settings watcher), and
`ui::em` rides the same base. **Anything the theme leaves unsized is `1em`** — a `.popup-menu-item`,
an app name under its icon — so it takes the base directly, not a `%caption`.

`PX_PER_PT` is the unit conversion only; the knob for "uniformly too large/small" is the base.
Live-confirmed 2026-07-27: on a `Cantarell 12` desktop our text read ~9% short of GNOME's until the
base was followed.

### 1.3.1 Font family — Cantarell everywhere

GNOME renders **all** shell UI in one family, from `org.gnome.desktop.interface font-name`
(default **"Cantarell 11"**), set on the Clutter `stage` and inherited by every actor
(`_common.scss:68` `stage { … color: $fg_color }` + St's default font). There is no per-widget
family in the theme — panel, popovers, dialogs, notifications, calendar, quick-settings, OSD, MRU
all inherit Cantarell. So matching GNOME's look is not per-widget: **the family is a single global
choice, and it must be Cantarell.** (Monospace spans — dialog command echoes, keycaps — use
`monospace-font-name`, default "Source Code Pro 10"; we map those to the generic monospace.)

**Our implementation** (`niri-vk/src/text.rs`):
- `pub const SANS_FAMILY = Family::Name("Cantarell")` — every sans shape/measure names it. Do **not**
  use `Family::SansSerif`: fontconfig resolves the generic to whatever `fc-match sans` returns (Noto
  Sans on this VM), a different typeface whose glyph shapes (the "J"/"l") and metrics differ from
  GNOME. cosmic-text falls back to the fontdb default if Cantarell is absent (GNOME systems always
  ship it).
- **Tabular figures.** Cantarell's default digits are *proportional* (`1` narrower than `8`), which
  jitters the advance-centered clock every second. GNOME fixes this with `%numeric` =
  `font-feature-settings: "tnum"` on the panel + calendar (§1.2). We match it in `sans_label_attrs`:
  the single-line **label** shape+measure path enables `tnum` (cosmic-text 0.19
  `Attrs::font_features` + `FeatureTag::new(b"tnum")`); **body paragraphs stay proportional** (they
  don't set it), mirroring GNOME's scoping. Pinned by `panel::tests::clock_advance_width_is_stable_across_seconds`.
- **Weight.** Our rasterizer tops out at bold (700); GNOME's `%title_1`/`%title_2` are 800 → drawn
  bold. `%heading`/`%title_3`/`%title_4` are 700 (exact).

**TODO (fork tenet "GNOME's way"):** read `font-name` / `monospace-font-name` from
`org.gnome.desktop.interface` at runtime instead of hardcoding Cantarell, so a user's font choice is
honored (family + the base pt that feeds §1.3). Hardcoding Cantarell is the faithful *default* only.

### 1.4 Colors

Accent palette — `src/st/st-theme-context.c:31-39`. The **foreground on any accent is hardcoded
`#ffffff`** for every accent, including light ones like yellow (`:41`, applied `:298`) — there is no
luminance selection.

| Accent | hex | | Accent | hex |
|---|---|---|---|---|
| blue (default) | `#3584e4` | | red | `#e62d42` |
| teal | `#2190a4` | | pink | `#d56199` |
| green | `#3a944a` | | purple | `#9141ac` |
| yellow | `#c88800` | | slate | `#6f8396` |
| orange | `#ed5b00` | | **accent-fg** | **`#ffffff`** |

Semantic colors (dark variant) — `_default-colors.scss`:

| Role | value (dark) | Source |
|---|---|---|
| base bg | `#222226` | `_default-colors.scss:4` |
| destructive / error bg | `$red_4 #c01c28` | `:11,:24` |
| destructive/error fg | `#ffffff` | `:12,:25` |
| success bg | `$green_5 #26a269` | `:16` |
| warning bg | `#cd9309` | `:20` |
| warning fg | `rgba(0,0,0,.8)` | `:21` |
| `$background_mix_factor` | 9% (dark) | button bg = fg mixed into bg at this % — `:33` |
| `$shadow_color` | `rgba(0,0,0,.2)` | `:36` |
| `$border_opacity` | .9 (dark) | `:40` |

Neutral palette (`_palette.scss:37-46`): `light_1..5` `#ffffff #f6f5f4 #deddda #c0bfbc #9a9996`,
`dark_1..5` `#77767b #5e5c64 #3d3846 #241f31 #000000`.

### 1.5 Button primitives (`%button` family)

The single most-reused primitive; `.icon-button`, quick-toggles, dialog buttons all derive from it.
Resolved from `_common.scss:93-231` + the `button()` mixin `_drawing.scss:160`. See the **buttons**
section below for the full per-state table; the short version:

| State | dark-variant background | Source |
|---|---|---|
| normal | subtle raised gray (fg→bg @ 9%) | `_drawing.scss:171` |
| hover | +4% lighter | `_drawing.scss:193` |
| active | +9% lighter | `:194` |
| checked | **accent fill, `#fff` fg** (`%default_button`) | `_common.scss` |
| insensitive | darkened / greyed | `:196` |
| flat | transparent until hover | `_drawing.scss` |

`.icon-button` = a `%button` forced circular (`border-radius: $forced_circular_radius`), `16px` icon
+ `$scaled_padding*2` = 12px padding each side → **40px** disc. `_buttons.scss:18`.

---

## 1.9 Two padding families: fixed px vs em-scaled

Not every "6px" in the theme is 6px. Two different tokens are in play, and they behave differently
when the user changes their font size:

- **`$base_padding: 6px`** and friends are literal px. `.message`'s padding, `.message-box`'s
  padding, `.message-header-content`'s `padding-bottom` — all fixed.
- **`$scaled_padding: to_em(6px)`** is `0.409em` (`_drawing.scss:6-10`; the `1.091` factor is
  tuned so it *equals* 6px at the default `$base_font_size: 11pt`). `%card_common`'s
  `padding: $scaled_padding * 2` therefore renders **12px at 11pt and 13px at 12pt**.

**Measured, not derived** (`tools/gnome-ui-dump` against a live 50.3 session running Cantarell 12):
`.world-clocks-button` and `.events-button` report `padding: [13,13,13,13]`, while
`.datemenu-today-button` — which overrides with the fixed `$base_padding * 1.5` — reports `[9,9,9,9]`
regardless.

**Our divergence:** we hardcode these as px at the 11pt value (`EVENTS_CARD_PAD = 12`), so we are
correct at the default font and drift under any other. Fixing it properly means threading the font
size into the card metrics the way `ui::pt_to_px` already does for text. Not yet done; recorded here
so a future "the bubbles look tight at large text" report has an explanation waiting.

Note this is also why the *outer* radius is a different kind of value: `.datemenu-popover`'s
`border-radius: $base_border_radius * 1.5 + $base_padding * 3` = 30px is plain px and does not move.

## 2. Components

Organized to mirror `data/theme/gnome-shell-sass/widgets/*.scss` — one subsection per widget file, so
the map is complete for the full-shell port. These tables are the **GNOME reference** (dark variant);
our implementation state and per-element constant mapping live in the §3 ledger.

## 2.1 Shared primitives — buttons, entries, controls
*Every other widget reuses these. `_buttons.scss`, `_entries.scss`, `_search-entry.scss`, `_check-box.scss`, `_switches.scss`, `_slider.scss`, `_scrollbars.scss`, `_base.scss`.*

### buttons
Styles all `.button` (text) and `.icon-button` (circular icon-only), incl `.default` (accent) and `.flat`, via the shared `%button` family.

| Element | CSS class | Font pt/wt | fg / bg / border | Radius | Padding | Source |
|---|---|---|---|---|---|---|
| Text button, normal | `.button` (`%button`+`button(normal)`) | 11pt/700 | fg #fff; bg subtle raised gray (fg→bg @9%) | 8px | `3px 24px` | _buttons.scss:3; _common.scss:93-103; _drawing.scss:160-171 |
| — hover | `.button:hover` | | bg +4% lighter | 8px | | _drawing.scss:193 |
| — active/selected | `.button:active/:selected` | | bg +9% lighter | 8px | | _drawing.scss:194 |
| — checked | `.button:checked` | | bg +8% lighter | 8px | | _drawing.scss:195 |
| — insensitive | `.button:insensitive` | | fg fg@50%; bg −3% darker | 8px | | _drawing.scss:196 |
| — focus | `.button:focus` | | inset ring 2px accent @80%; bg accent-mixed 5% | 8px | | _drawing.scss:309-327 |
| Default/suggested, normal | `.button.default` (`%default_button`) | 11pt/700 | fg **#ffffff**; bg accent solid | 8px | `3px 24px` | _buttons.scss:9; _common.scss:122-128 |
| Flat, normal | `.button.flat` (`%flat_button`) | 11pt/700 | fg #fff; bg ambient (reads transparent) | 8px | `3px 24px` | _buttons.scss:13; _common.scss:112-120 |
| — flat hover | `.button.flat:hover` | | bg +7% lighter (stronger) | 8px | | _drawing.scss:188-190 |
| Icon button | `.icon-button` (`%button`, circular) | icon | same state deltas as `.button` | `999px` | `$scaled_padding*2`=12px each side | _buttons.scss:18-28 |
| Icon button icon | `.icon-button StIcon` | | symbolic | | | 16px | _buttons.scss:24 |
| min-height | | | | | text `to_em(22px)`; icon 16px | _buttons.scss:6,23 |

### entries
Base `StEntry` text-input primitive, reused by every text field.

| Element | CSS class | Font | fg / bg / border | Radius | Padding | Source |
|---|---|---|---|---|---|---|
| Entry, normal | `StEntry` (`%entry`+`entry(normal)`) | 11pt/400 | fg fg@70%; bg mix(fg,bg,9%) | 8px | `9px 9px` | _entries.scss:3; _common.scss:175-193 |
| — hover | `:hover` | | fg full; bg +4% | 8px | | _drawing.scss:146-149 |
| — focus | `:focus` | | inset ring 2px accent @80%; bg accent-mixed 5% | 8px | | _drawing.scss:133-143 |
| — insensitive | `:insensitive` | | fg fg@50%; bg −3% | 8px | | _drawing.scss:151-155 |
| Selection | | | selection-bg accent@30%; text fg | | | _common.scss:179-180 |
| Capslock/peek icons | `.capslock-warning`/`.peek-password` | | warning color / inherit | | `0 4px` | _entries.scss:6-15 |
| Hint text | `.hint-text` | | fg@70% | | margin-left 2px | _entries.scss:17-19 |

### search-entry
Pill-shaped Overview search box; a "system" (always-dark) entry.

| Element | CSS class | Font | fg / bg / border | Radius | Padding | Source |
|---|---|---|---|---|---|---|
| Search entry | `.search-entry` (`%system_entry`, always_dark) | 11pt/400 | fg system_fg@70% #fafafb; bg mix(system_fg,system_bg,9%) | `999px` | `9px 9px`; margin-top 12px, bottom 6px; width 24em | _search-entry.scss:1-9; _common.scss:337-345 |
| — focus | `:focus` | | inset ring 2px accent @80% | 999px | | _common.scss:341 |
| Search icon | `.search-entry-icon` | | inherit | | `0 4px`; margin-top 2px | 16px | _search-entry.scss:10-14 |

### check-box
`CheckBox` (St.Button subclass): labeled row + small square glyph frame.

| Element | CSS class | fg / bg / border | Radius | Size/pad | Source |
|---|---|---|---|---|---|
| Label spacing | `.check-box StBoxLayout` | — | | spacing .8em | _check-box.scss:4 |
| Focus ring | `.check-box:focus StBin` | inset 0 0 0 2px accent @35% | 7px | 2px | _check-box.scss:11-15 |
| Glyph box, unchecked | `.check-box StIcon` | fg transparent; border 2px white@15% | 6px | 1px, icon 14px | _check-box.scss:17-24 |
| — hover/active | `:hover`/`:active StIcon` | border white@20% / @30% | 6px | | _check-box.scss:26-32 |
| — checked | `:checked StIcon` | bg accent; fg **#fff**; border transparent | 6px | | _check-box.scss:34-38 |

### switches
`.toggle-switch` pill track + circular handle used throughout quick settings.

| Element | CSS class | fg / bg / border | Radius | Size | Source |
|---|---|---|---|---|---|
| Track, off | `.toggle-switch` | bg white@15% | `999px` | width 46px; 100ms | _switches.scss:6-19 |
| — off hover | `:hover` | bg white@20% | 999px | | _switches.scss:21-23 |
| Glyph icons | `.toggle-switch StIcon` | inherit | | 16px (fixed) | _switches.scss:26-28 |
| Handle, off | `.handle` | bg near-white (mix white,bg,80%); shadow 0 2px 4px black@20% | 999px | 20×20px; margin 3px | _switches.scss:30-38 |
| Track, on | `:checked` | fg accent-fg; bg accent | 999px | | _switches.scss:40-43 |
| Handle, on | `:checked .handle` | bg **#ffffff** | 999px | 20×20px | _switches.scss:49-51 |

### slider
`Slider` (St.Slider/BarLevel) for volume/brightness.

| Element | CSS class | fg / bg / border | Radius | Size | Source |
|---|---|---|---|---|---|
| Slider | `.slider` | color darken(fg,9%); trough bg white@10% | handle radius 8px (`$slider_size*0.5`) | icon 16px | _slider.scss:1-17 |
| Trough fill | `.slider` (`-barlevel-*`) | height 4px; fill accent; overdrive `#c01c28` (1px sep) | | | _slider.scss:9-15 |
| — hover | `:hover` | color full fg | | | _slider.scss:24-26 |

### scrollbars
`StScrollView` fade edges and `StScrollBar` overlay handle.

| Element | CSS class | fg / bg / border | Radius | Size | Source |
|---|---|---|---|---|---|
| Fade | `.vfade`/`.hfade` | — | | offset 68px | _scrollbars.scss:4-5 |
| Bar | `StScrollBar` | — | | 8×8px min | _scrollbars.scss:8-14 |
| Trough | `#trough` | transparent | 0 | | _scrollbars.scss:16-19 |
| Handle, normal | `.vhandle`/`.hhandle` | bg mix(fg,bg,30%); border 3px transparent (inset) | 8px | 500ms | _scrollbars.scss:21-25 |
| — hover/active | `:hover`/`:active` | bg mix 50% / 40% | 8px | | _scrollbars.scss:26-27 |

### base
Generic utilities: hyperlink color + icon shadow presets.

| Element | CSS class | fg / effect | Source |
|---|---|---|---|
| Link | `.shell-link` | color `$link_color` (lighten accent 20%); hover +10% | _base.scss:2-7 |
| Low-res icon outline | `.lowres-icon` | icon-shadow 0 1px 2px black@20% | _base.scss:11-13 |
| Icon dropshadow | `.icon-dropshadow` | icon-shadow 0 2px 4px black@40% | _base.scss:16-22 |

## 2.2 Top panel & quick settings

### panel
Styles the top bar (`#panel`) and its buttons: activities/workspace dots, clock, screen-recording/sharing indicators, status-icon tray.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon size | Source |
|---|---|---|---|---|---|---|---|
| Top bar | `#panel` | bold, `%numeric` (tabular) | bg dark `$dark_5` #000000 / light #fafafb | none (light: 0.5px inset bottom border) | height `2.2em` (~32px @11pt) | — | panel.scss:13-23 |
| Panel button (base) | `.panel-button` (`panel_button()`) | bold | fg dark #ffffff/light #222226; bg transparent; hover fg@17%; active/checked fg@28%; active+hover fg@32% | fully rounded (999px) | 3px transparent border; `-natural-hpadding: 12px`, `-minimum-hpadding: 6px` | — | panel.scss:26-45; drawing.scss:393-437 |
| Status tray icon | `.system-status-icon` | inherit | inherit | — | `padding: 0 6px; margin: 0 4px` | 16px | panel.scss:32-36 |
| Combined status box | `.panel-status-indicators-box` | — | — | — | spacing 4px in `.panel-button`; 2px at top level; `.power-status` spacing 0 | — | panel.scss:39-40,145-152; panel.js:292 |
| Activities workspace dot | `#panelActivities .workspace-dot` | — | bg dark #ffffff | fully rounded | min 8px (`$scalable_icon_size*0.5`); parent padding `0 3px`, spacing 5px | — | panel.scss:47-58; panel.js:64 |
| Screen-recording indicator | `.panel-button.screen-recording-indicator` (`panel_button(filled)`) | bold | fg #fafafb; bg fill `$red_4` #c01c28 (hover +5%, active +9%) | fully rounded | 3px transparent border; inner spacing 6px | 16px | panel.scss:62-75 |
| Screen-sharing indicator | `.panel-button.screen-sharing-indicator` (`panel_button(filled)`) | bold | fg #fafafb; bg fill `$orange_3` #ff7800 | fully rounded | as above | 16px | panel.scss:62-79 |
| Clock button | `.panel-button.clock-display` (`panel_button(highlighted_child, child=.clock)`) — fill moves to `.clock` child so the DND dot isn't covered | bold | fg panel_fg; child `.clock` gets hover/active fills (transparent/fg@17%/fg@28%) | `.clock`: fully rounded, 3px transparent border | — | — | panel.scss:81-95,116-143; drawing.scss:439-469; dateMenu.js:865 |
| Clock label | `.clock` | inherit | inherit (highlighted child) | — | `padding-left/right: 12px` | — | panel.scss:159-165 |
| DND messages indicator | `.messages-indicator` | — | — | — | — | 16px | panel.scss:92-94; dateMenu.js:746 |
| Privacy-indicator text | `.privacy-indicator` | inherit | color `$privacy_indicator_color` dark #ff7800 | — | — | — | panel.scss:155 |
| Overview/lock/login panel | `#panel:overview`, `.unlock-screen`, `.login-screen` | inherit | bg transparent; `.panel-button` fg → #ffffff (unlock/login) / #fafafb (overview) | — | — | — | panel.scss:7-9,98-143 |

### quick-settings
Styles the Quick Settings popup menu: the toggle-tile grid, quick-toggle pills (+ menu-split variant), sliders, per-toggle submenus, and the system/network/bluetooth/background-apps rows.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon size | Source |
|---|---|---|---|---|---|---|---|
| QS menu container | `.quick-settings` | inherit | popup menu bg | `36px` (`$modal_radius*2.25`) | `18px` (`$base_padding*3`) | — | quick-settings.scss:1-8; quickSettings.js:760 |
| Icon/plain buttons in menu | `.quick-settings .icon-button/.button` | inherit | inherit | inherit | `10.5px` (`$base_padding*1.75`) | — | quick-settings.scss:5-7 |
| Toggle grid | `.quick-settings-grid` | — | — | — | row/col spacing `12px` | — | quick-settings.scss:10-13 |
| Quick-toggle tile (normal) | `.quick-toggle`, `.quick-toggle-has-menu` | inherit | fg default-button fg; bg normal `%button` (fg→bg @9%, subtle raised gray) | fully rounded (999px) | min/max-w `12em`; min-h `48px` (`$scalable_icon_size*3`); inner spacing `9px`, padding `0 12px` (+15px leading) | — | quick-settings.scss:15-36; quickSettings.js:57 |
| Quick-toggle (`:checked`) | `.quick-toggle:checked` (`%default_button`) | inherit | fg `-st-accent-fg-color` **#ffffff**; bg accent (all states) | fully rounded | as above | — | quick-settings.scss:23-25 |
| Toggle title | `.quick-toggle-title` (`%heading`) | **11pt / 700** | inherit | — | — | — | quick-settings.scss:38-40; quickSettings.js:91 |
| Toggle subtitle | `.quick-toggle-subtitle` (`%caption`, normal wt) | 9pt / 400 | inherit | — | — | — | quick-settings.scss:42-45 |
| Toggle icon | `.quick-toggle-icon` | — | inherit | — | — | 16px | quick-settings.scss:47 |
| Menu-split arrow button | `.quick-toggle-menu-button.icon-button` | inherit | fg `$fg_color`; bg dark `lighten(bg,8%)` | ltr `0 999px 999px 0` (outer corner only) | `6px 10.5px` | — | quick-settings.scss:50-79 |
| Menu-split arrow (`:checked`) | `.quick-toggle-menu-button:checked` | inherit | fg **#ffffff**; bg near-solid accent tint | same | same | — | quick-settings.scss:96-124 |
| Toggle-pair separator | `.quick-toggle-separator` | — | normal fg@25%; checked accent tint | — | width `1px` | — | quick-settings.scss:81-83 |
| Quick-slider row | `.quick-slider` | — | — | — | box spacing `6px` | — | quick-settings.scss:138-139 |
| Slider bin | `.slider-bin` | — | `:focus` → button focus ring | fully rounded (999px) | `6px` | — | quick-settings.scss:143-147 |
| Per-toggle submenu | `.quick-toggle-menu` (`%card`) | inherit | bg `$card_bg_color` dark `lighten(bg,7%)`; 1px border + shadow (transparent in dark) | `24px` (`$base_border_radius*3`) | margin `12px 0 0` | — | quick-settings.scss:150-160 |
| Submenu header icon (normal) | `.quick-toggle-menu .header .icon` | — | bg fg@20% | fully rounded (999px) | `9px` | 24px (`$medium_scalable_icon_size`) | quick-settings.scss:172-177 |
| Submenu header icon (`.active`) | `.header .icon.active` | — | fg **#ffffff**; bg accent | same | same | 24px | quick-settings.scss:178-181 |
| Submenu header title | `.header .title` (`%title_3`) | 15pt / 700 | inherit | — | — | — | quick-settings.scss:192-194 |
| Submenu header subtitle | `.header .subtitle` (`%caption_heading`) | 9pt / 700 | inherit | — | — | — | quick-settings.scss:196-198 |
| System row container | `.quick-settings-system-item` | — | — | — | box spacing `12px` | — | quick-settings.scss:205-206; system.js:266 |
| System power/lock icon buttons | `.quick-settings-system-item .power-item.icon-button` | — | inherit; insensitive bg transparent | — | min-w/h `0` | (icon-button 16px) | quick-settings.scss:208-216 |
| Network item secure icon | `.nm-network-item .wireless-secure-icon` | — | — | — | — | 8px (`$scalable_icon_size*0.5`) | quick-settings.scss:219-221 |
| Device subtitle | `.device-subtitle` | — | fg@50% | — | — | — | quick-settings.scss:234 |
| Bluetooth empty placeholder | `.bt-menu-placeholder.popup-menu-item` (`%title_4`) | 13pt / 700 | inherit centered | — | `2em 4em` | — | quick-settings.scss:227-232 |
| Keyboard-brightness `.button:checked` | (`%default_button`) | inherit | fg **#ffffff**; bg accent | fully rounded | — | — | quick-settings.scss:239 |
| Background-app row title | `.background-app-item .title` (`%heading`) | 11pt / 700 | inherit | — | — | — | quick-settings.scss:252 |
| Background-app row subtitle | `.background-app-item .subtitle` (`%caption`) | 9pt / 400 | inherit | — | — | — | quick-settings.scss:253 |
| Background-app row icon | `.background-app-item .popup-menu-icon` | — | regular (non-symbolic) | — | — | 32px `!important` | quick-settings.scss:254-257 |

## 2.3 Menus, dialogs, notifications, OSD

### popovers
GNOME's popup-menu (right-click menus, panel dropdowns, app menu): container, items, separators, submenus.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon | Source |
|---|---|---|---|---|---|---|---|
| Boxpointer (arrow) | `.popup-menu-boxpointer` | — | — | — | `-arrow-rise: 6px` | — | _popovers.scss:11-13 |
| Menu container | `.popup-menu` | 11pt/400 | fg #fff | — | min-width 15em; `.panel-menu` margin-bottom 1.75em | — | _popovers.scss:16-24 |
| Content box | `.popup-menu-content` | | fg inherit / bg `#36363a` / border 1px `#424247`; shadow 0 2px 4px black@20% | `20px` (`$modal_radius*1.25`) | padding 6px | — | _popovers.scss:26-33 |
| Menu item (normal) | `.popup-menu-item` (`menuitem`+flat undecorated) | 11pt/400 | fg inherit; bg transparent | `12px` | `9px 12px`; spacing 6px | — | _popovers.scss:36-48 |
| — hover/selected | `.popup-menu-item:hover` | | bg lighten 4% (flat) | 12px | | — | _drawing.scss:386-389 |
| — checked (open submenu) | `.popup-menu-item:checked` | | | `12px 12px 0 0` | | — | _popovers.scss:41-48 |
| Inactive item (label) | `.popup-inactive-menu-item` | | fg #fff; insensitive `#9a9a9c` | | | — | _popovers.scss:63-66 |
| Menu icon/arrow | `.popup-menu-arrow`/`.popup-menu-icon` | | | | | 16px | _popovers.scss:69-72 |
| Submenu container | `.popup-sub-menu` | 11pt/400 | bg lighten(bg,13%) `#56565c` | bottom `13px` | margin-bottom 6px | — | _popovers.scss:75-95 |
| Ornament (check/radio) | `.popup-menu-ornament` | | | | width 16px | 16px | _popovers.scss:108-111 |
| Separator | `.popup-separator-menu-item-separator` | | bg rgba(255,255,255,.1) | | height 1px | — | _popovers.scss:114-134 |
| App/right-click menu | `.app-menu` | | | | max-width 27.25em | — | _popovers.scss:143-154 |
| "Open Windows" caption | `.app-menu … StLabel` (`%caption_heading`) | 9pt/700 | | | margin 8px | — | _popovers.scss:146-153 |

### dialogs
Modal dialog chrome: end-session, message/confirm, run dialog, password/polkit, audio-selection, access-portal.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Source |
|---|---|---|---|---|---|---|
| Dialog container | `.modal-dialog` | inherit | bg `#36363a` | `18px` (`$alert_radius`) | padding 24px; spacing 18px; shadow 0 12px 8px 12px black@3% | _dialogs.scss:4-9 |
| Content box | `.modal-dialog-content-box` | | | | padding-top 12px; spacing 18px; max-width 28em | _dialogs.scss:11-15 |
| Button box | `.modal-dialog-button-box` | | | | padding-top 6px | _dialogs.scss:17-18 |
| Dialog button (`%dialog_button`) | `.modal-dialog-button` | 11pt/700 | fg #fff; bg white@10%, hover @13%, active @16% | `12px` | padding 12px | _dialogs.scss:19-22; _common.scss:211-222 |
| Dialog list title (`%heading`) | `.dialog-list-title` | 11pt/700 | centered | | | _dialogs.scss:29-31 |
| Dialog list item title | `.dialog-list-item-title` | 11pt/700 | | | | _dialogs.scss:41 |
| Dialog list item desc (`%caption`) | `.dialog-list-item-description` | 9pt/400 | fg ≈`#e6e6e6` | | | _dialogs.scss:42-45 |
| End-session dialog | `.end-session-dialog` | | | | width 24em | _dialogs.scss:51-52 |
| Battery/warning banner | `.end-session-dialog-battery-warning` | inherit | fg `#cd9309`; bg warning@10% | `8px` | padding 9px; margin 4px 0 | _dialogs.scss:54-62 |
| Message dialog title (`%title_2`) | `.message-dialog-title` | 15pt/800 centered | | | | _dialogs.scss:69-72 |
| — lightweight (`%title_4`) | `.message-dialog-title.lightweight` | 13pt/700 | | | | _dialogs.scss:73-75 |
| Message dialog desc | `.message-dialog-description` | inherit centered | | | | _dialogs.scss:77-79 |
| Run dialog | `.run-dialog` | | | | width 24em | _dialogs.scss:83-88 |
| Run dialog entry | `.run-dialog-entry` | | | | padding 12px 9px | _dialogs.scss:90-92 |
| Run dialog desc (`%caption`) | `.run-dialog-description` | 9pt/400 | fg darken(fg,20%) | | | _dialogs.scss:93-96 |
| Prompt (password) dialog | `.prompt-dialog` | | | | width 28em | _dialogs.scss:100-101 |
| Password entry | `.prompt-dialog-password-entry` | | | | width 20em; padding 12px 9px | _dialogs.scss:107-122 |
| Prompt error/info (`%caption`) | `.prompt-dialog-error-label`/`-info-label` | 9pt/400 centered | error fg `#cd9309`/bg warning@10%; info fg #fff/bg white@10% | `8px` | padding 9px; margin 4px 0 | _dialogs.scss:124-142 |
| Polkit user label (`%title_4`) | `.polkit-dialog-user-label` | 13pt/700 | | | | _dialogs.scss:151-154 |
| Polkit root label (`%title_4`) | `.polkit-dialog-user-root-label` | 13pt/700 | fg `#cd9309` | | | _dialogs.scss:156-158 |
| Audio-selection device tile | `.audio-selection-device` (flat tile_button) | | flat/transparent, hover/active | `16px` | padding 12px; spacing 12px | _dialogs.scss:165-179 |
| Restart message (`%title_4`) | `.restart-message` | 13pt/700 | | | | _dialogs.scss:199-201 |

### notifications
Transient banner notification popup (top-of-screen) + action buttons.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Source |
|---|---|---|---|---|---|---|
| Banner | `.notification-banner` | inherit | | `16px` (`$modal_radius`) | margin 4px; min-height 64px; width 34em; shadow 0 2px 4px 2px black@20% | _notifications.scss:7-17 |
| Action button (`%notification_button`) | `.notification-button` | 11pt/700 | fg #fff; bg white@15%, hover @30%, active @20% | `8px` | padding 6px 12px | _notifications.scss:23-25; _common.scss:197-208 |

Note: banner content (source icon/title/body) uses the shared `.message` classes in message-list.

### message-list
Calendar/QS notification history: message "cards" + inline media controls.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon | Source |
|---|---|---|---|---|---|---|---|
| List container | `.message-list` | inherit | border 1px rgba(255,255,255,.1) | — | width 29em | — | _message-list.scss:6-13 |
| Empty placeholder (`%title_3`) | `.message-list-placeholder` | 15pt/700 | fg white@45% | | icon margin-bottom 12px | 96px | _message-list.scss:14-26 |
| DND/clear-all bar (`%heading`) | `.message-list-controls` | 11pt/700 | | | padding 12px; spacing 6px | — | _message-list.scss:44-49 |
| Clear-all button | `.message-list-clear-button` | | | fully rounded | | — | _message-list.scss:51-53 |
| Group title (`%title_2`) | `.message-group-title` | 15pt/800 | | | margin 0 4px | — | _message-list.scss:62-65 |
| Collapse button | `.message-collapse-button` (`.icon-button`) | | fg #fff; bg white@20% | icon-button | padding 4px; border 4px transparent | — | _message-list.scss:69-77 |
| Message bubble (`%card`) | `.message` | | fg inherit; bg ≈`#51515a`; border transparent | `16px` | padding 6px | — | _message-list.scss:81-97 |
| Header row | `.message-header` | | fg `#b1b1b3` | | spacing 6px; padding 0 6px | — | _message-list.scss:101-109 |
| Source icon | `.message-source-icon` | | symbolic | | | 16px | _message-list.scss:111-114 |
| Source title | `.message-source-title` | 11pt/700 | | | | — | _message-list.scss:123-125 |
| Event time (`%caption`) | `.event-time` | 9pt/400 | fg `#b1b1b3` | | text-align end | — | _message-list.scss:128-135 |
| Expand/close (`%notification_button`+`.icon-button`) | `.message-expand-button`/`-close-button` | 11pt/700 | fg #fff; bg white@15% | fully rounded | padding 6px; close margin 3px | — | _message-list.scss:139-156 |
| Message icon (large) | `.message-icon` | | | | margin-right 6px | 48px | _message-list.scss:165-171 |
| Themed icon (symbolic-on-circle) | `.message-themed-icon` | | fg inherit; bg white@7% | fully rounded | min 48px box | 16px | _message-list.scss:174-180 |
| Message title | `.message-title` | 11pt/700 | | | | — | _message-list.scss:193-195 |
| URL highlighter | `.url-highlighter` | | fg `$link_color` | | | — | _message-list.scss:222-223 |
| Media control button | `.message-media-control` | inherit | fg #fff; hover/active shaded | `8px` | padding 0 18px | 16px | _message-list.scss:226-257 |

### osd
On-screen-display bubble (volume/brightness), monitor-number label, pad-OSD.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon | Source |
|---|---|---|---|---|---|---|---|
| OSD bubble (`%osd_panel`+`%heading`) | `.osd-window` | 11pt/700 centered | fg #fff; bg `#2e2e33` (`$osd_bg_color`); border 1px white@2% | fully rounded (999px) | padding 12px 18px; spacing 12px; children 8px; margin-bottom 4em | — | _osd.scss:5-20 |
| OSD icon | `.osd-window StIcon` | | | | | 32px (`$large_icon_size`) | _osd.scss:15 |
| OSD level bar | `.level` (BarLevel) | | fill #fff; track white@10%; overdrive `#c01c28` | | height 6px; min-width 160px | — | _osd.scss:22-34 |
| Monitor-number / countdown | `.osd-monitor-label`/`.osd-break-countdown-label` | 3em/700 tnum centered | fg #fff (accent-fg); bg accent | `16px` | margin 12px; padding 12px; min-width 1.5em | — | _osd.scss:38-49 |
| Pad-OSD window | `.pad-osd-window` | | bg black@80% | | padding 32px | — | _osd.scss:52-55 |
| Resize popup (`%osd_panel`) | `.resize-popup` | inherit | fg #fff; bg `#2e2e33` | fully rounded | padding 12px | — | _osd.scss:64-66 |

_(All colors are the default **dark** variant: `$bg_color #36363a`, `$fg_color #ffffff`, `$osd_bg_color #2e2e33`.)_

## 2.4 Calendar, switchers, screenshot UI

### calendar

Styles the date/time menu: the mini month calendar grid, the today-button, and the events/world-clocks/weather cards in the calendar popover column.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon size | Source |
|---|---|---|---|---|---|---|---|
| Calendar popover container | `.datemenu-popover` | inherit | inherit | `8px*1.5 + 6px*3 = 30px` | — | — | _calendar.scss:8-10 |
| Calendar side column | `.datemenu-calendar-column` | inherit | inherit | — | spacing `6px`; margin-left/right `6px` | — | _calendar.scss:13-20 |
| Today button (date) | `.datemenu-today-button` (`%card_flat`→`%card_common`+flat `button()`) | body 11/400 | fg `$fg_color` #ffffff (dark); bg `$card_bg_color` = lighten(#36363a,7%) ≈ `#47474c`; border 1px transparent | `8px*1.5=12px` | padding `6px*1.5=9px`; margin `4px` | — | _calendar.scss:23-25; js dateMenu.js:58 |
| Today button weekday label | `.day-label` | 11/700 (bold, inherits body size) | inherit fg | — | inherit | — | _calendar.scss:28-30; js dateMenu.js:71 |
| Today button date label | `.date-label` (`%title_2`) | 15/800 | inherit fg | — | inherit | — | _calendar.scss:33-35; js dateMenu.js:76 |
| Mini calendar card | `.calendar` (`%card_flat`) | inherit | fg `$fg_color` #ffffff; bg `$card_bg_color` ≈`#47474c`; border transparent | `12px` (card_common) | padding `0`; margin-top `0` | — | _calendar.scss:39-42; js calendar.js:443 |
| Month header prev/next icons | `.calendar-change-month-back/-forward StIcon` | — | inherit | — | — | `$scalable_icon_size` = 16px | _calendar.scss:48-51; js calendar.js:503,521 |
| Month label | `.calendar-month-label` (`%heading`+`%flat_button`) | 11/700 | fg `$fg_color` forced `!important`; bg = flat button normal | `$forced_circular_radius` = 999px (pill) | `8px 0`; width `10em` | — | _calendar.scss:54-62; js calendar.js:512 |
| Pager button (prev/next wrapper) | `.pager-button` (`.icon-button`, `.flat`) | — | flat-button colors | pill via `.icon-button`/flat default | `0`; size `2.6em × 2.6em` | base icon-button size | _calendar.scss:64-69 |
| Day cell (normal) | `.calendar-day` (`%numeric`+`%smaller`+`%flat_button`) | 9/400, tabular-nums | flat-button fg/bg (transparent normal, hover/active mixed) | `999px` (circle) | margin `2px`; padding `0`; size `3em × 3em` | — | _calendar.scss:73-84; js calendar.js:691 |
| Day cell — weekend | `.calendar-day.calendar-weekend` | 9/400 | fg `$insensitive_fg_color` (dark) ≈ `#9b9b9d` | 999px | as `.calendar-day` | — | _calendar.scss:87-92; js calendar.js:696 |
| Day cell — other month | `.calendar-day.calendar-other-month` | 9/400 (not bold) | fg `transparentize($fg_color,0.5)` = rgba(255,255,255,.5) | 999px | as `.calendar-day` | — | _calendar.scss:94-109; js calendar.js:711 |
| Day cell — today | `.calendar-day.calendar-today` (`%default_button`) | 9/700 bold | fg `-st-accent-fg-color` = `#ffffff` (forced); bg = accent default-button bg | 999px | as `.calendar-day` | — | _calendar.scss:111-118; js calendar.js:709 |
| Day cell — has-events dot | `.calendar-day.calendar-day-with-events` | inherit | bg-image `calendar-today.svg` (dark asset), `background-size: contain` | 999px | as `.calendar-day` | — | _calendar.scss:120-123; js calendar.js:714 |
| Day cell — today + has-events | `.calendar-today.calendar-day-with-events` | as today | forces light dot asset `!important` | 999px | — | — | _calendar.scss:115-117 |
| Weekday heading row | `.calendar-day-heading` (`%numeric`+`%smaller`+`%flat_button`) | 9/700 bold | flat-button fg/bg | `8px` | margin `4px`; padding `3px 6px` | — | _calendar.scss:127-136; js calendar.js:542 |
| Week-number column | `.calendar-week-number` (`%smaller`) | 9/700 bold, tabular-nums | fg `$insensitive_fg_color`; bg `transparentize(...,.8)` | `8px*0.5=4px` | margin `6px`; padding `0 6px` | — | _calendar.scss:139-149; js calendar.js:731 |
| Events/world-clocks/weather card | `.events-button`/`.world-clocks-button`/`.weather-button` (`%card`) | inherit | fg `$fg_color`; bg `$card_bg_color` (+hover/active/focus states) | `12px` | padding `12px`; margin `4px` | — | _calendar.scss:153-157; js dateMenu.js:115,335,547 |
| Events title | `.events-title` (`%heading`) | 11/700 | fg `$card_insensitive_fg_color` | — | padding-bottom `6px` | — | _calendar.scss:165-169; js dateMenu.js:132 |
| Event box (row) | `.event-box` | inherit | inherit | `8px` | spacing `6px` | — | _calendar.scss:176-178; js dateMenu.js:274 |
| Event summary | `.event-summary` (`%heading`) | 11/700 | fg inherit | — | — | — | _calendar.scss:180-182; js dateMenu.js:279 |
| Event time | `.event-time` (`%numeric`+`%caption`) | 9/400, tabular-nums | fg `$card_insensitive_fg_color` | — | — | — | _calendar.scss:184-187; js dateMenu.js:283 |
| Event placeholder ("No events") | `.event-placeholder` | body 11/400 italic | fg `$card_insensitive_fg_color` | — | — | — | _calendar.scss:191-194; js dateMenu.js:291 |
| World clocks header | `.world-clocks-header` (`%heading`) | 11/700 | fg `$card_insensitive_fg_color` | — | — | — | _calendar.scss:203-211; js dateMenu.js:415 |
| World clock time | `.world-clocks-time` (`%numeric`) | 11/700 bold, tabular-nums | inherit fg | — | — | — | _calendar.scss:223-228; js dateMenu.js:443 |
| Weather header | `.weather-header` (`%heading`) | 11/700; `.location` → normal | fg `$card_insensitive_fg_color` | — | — | — | _calendar.scss:249-261; js dateMenu.js:564 |
| Weather forecast temp | `.weather-forecast-temp` (`%numeric`) | 11/700 bold, tabular-nums | inherit | — | — | — | _calendar.scss:279-282; js dateMenu.js:659 |

### switcher-popup

Styles the Alt-Tab application/window switcher, its onscreen list panel and thumbnails, plus the input-source switcher and window-cycler highlight.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon size | Source |
|---|---|---|---|---|---|---|---|
| Full-screen switcher container | `.switcher-popup` | inherit | inherit (transparent) | — | padding `0`; spacing `24px` | — | _switcher-popup.scss:8-11; js switcherPopup.js:42 |
| Onscreen switcher panel | `.switcher-list` (`%osd_panel`) | inherit | fg `$osd_fg_color` #fff; bg `$osd_bg_color` ≈ `#2e2e33`; border 1px rgba(255,255,255,.02) | `16px+12px = 28px` | padding `12px` | — | _switcher-popup.scss:4-19; js switcherPopup.js:397 |
| Switcher panel shadow | `.switcher-list` | — | box-shadow `0 8px 8px 0 rgba(0,0,0,0.2)` | — | — | — | _switcher-popup.scss:18 |
| Item container | `.switcher-list-item-container` | inherit | inherit | — | spacing `12px` | — | _switcher-popup.scss:21-23 |
| App/window item box | `.item-box` (`tile_button(osd)`, flat) | inherit | fg #fff; bg flat-mix (normal ~transparent) | tile default | tile_button defaults | — | _switcher-popup.scss:26-30; js altTab.js:673 |
| Item box — selected | `.item-box:selected` | inherit | bg `transparentize(#fff,0.8)` = rgba(255,255,255,.2) | inherit | inherit | — | _switcher-popup.scss:32-40 |
| Separator | `.separator` | — | bg `$borders_color` rgba(255,255,255,.1) | — | width `1px` | — | _switcher-popup.scss:43-46 |
| Window thumbnail | `.thumbnail` | — | inherit | `8px` | width `256px` | — | _switcher-popup.scss:55-58; js altTab.js:927 |
| Multi-window arrow indicator | `.switcher-arrow` | — | rgba(255,255,255,.8); `:highlighted` → #fff | — | — | — | _switcher-popup.scss:62-70 |
| Input-source switcher glyph | `.input-source-switcher-symbol` | 34pt | inherit | — | — | box `96px × 96px` | _switcher-popup.scss:73-77 |
| Window-cycler selection outline | `.cycler-highlight` | — | border `5px solid -st-accent-color` | — | — | — | _switcher-popup.scss:80-82 |

### workspace-switcher

Styles the onscreen workspace-switcher OSD panel (the row of dots shown when switching workspaces).

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon size | Source |
|---|---|---|---|---|---|---|---|
| OSD panel container | `.workspace-switcher` (`%osd_panel`) | inherit | fg #fff; bg `$osd_bg_color` ≈ `#2e2e33`; border 1px rgba(255,255,255,.02) | `999px` (forced-circular) | padding `12px 18px`; margin-bottom `4em`; spacing `12px` | — | _workspace-switcher.scss:7-12; js workspaceSwitcherPopup.js:26 |
| Workspace dot — inactive | `.ws-switcher-indicator` | — | bg `transparentize(#fff,0.5)` = rgba(255,255,255,.5) | round | dot `5.33px` | — | _workspace-switcher.scss:3-18 |
| Workspace dot — active | `.ws-switcher-indicator:active` | — | bg solid #fff | round | dot `10.67px` | — | _workspace-switcher.scss:20-24 |

### screenshot

Styles the Screenshot UI overlay: the bottom control panel, capture/type/close buttons, area-selection handles/shade, window/screen selectors and tooltips.

| Element | CSS class (resolved @extend) | Font pt/wt | fg / bg / border | Radius | Padding / spacing | Icon size | Source |
|---|---|---|---|---|---|---|---|
| Bottom control panel | `.screenshot-ui-panel` (`%osd_panel`) | inherit | fg #fff; bg `$osd_bg_color` ≈`#2e2e33`; border 1px rgba(255,255,255,.02) | `16px*2=32px` | `18px` all sides, bottom `12px`; margin-bottom `4em`; spacing `12px` | — | _screenshot.scss:3-15; js screenshot.js:1240 |
| Close (X) button | `.screenshot-ui-close-button` (copies `.window-close`) | — | window-close colors | window-close default | `6px !important`; margins `12px` | — | _screenshot.scss:17-24 |
| Type toggle (Screen/Window/Selection) | `.screenshot-ui-type-button` (`%osd_button_flat`) | — | osd flat-button (always-dark mix) | `14px` | `12px 18px` | container icon `32px` | _screenshot.scss:26-38; js screenshot.js:1306 |
| Type button inner label | `.icon-label-button-container` (`%caption`) | 9/400 | inherit | — | spacing `6px` | icon `32px` | _screenshot.scss:32-37 |
| Capture button (outer ring) | `.screenshot-ui-capture-button` | — | border `4px solid #fff` | `999px` | padding `4px`; size `32px` | — | _screenshot.scss:40-45; js screenshot.js:1394 |
| Capture button inner circle | `.screenshot-ui-capture-button-circle` | — | bg #fff; hover outer → `#cccccc`; active → `#808080` | `999px` | 200ms | — | _screenshot.scss:47-64 |
| Capture button — cast state | `.screenshot-ui-capture-button:cast` | — | circle bg `$red_4` = `#c01c28` | `999px` | — | — | _screenshot.scss:66-80 |
| Shot/Cast segmented container | `.screenshot-ui-shot-cast-container` | — | bg rgba(255,255,255,.1) | `999px` | padding `3px`; spacing `3px` | — | _screenshot.scss:83-92 |
| Shot/Cast segment button | `.screenshot-ui-shot-cast-button` | — | transparent; `:checked` → bg #fff, fg `$osd_bg_color` | `999px` | `6px 12px` | `16px` | _screenshot.scss:95-110 |
| Area shade (dim outside selection) | `.screenshot-ui-area-indicator-shade` | — | bg `rgba(0,0,0,.3)`; inside selector `rgba(0,0,0,.5)` | — | — | — | _screenshot.scss:117-124 |
| Area selection outline | `.screenshot-ui-area-indicator-selection` | — | border `2px solid white` | — | — | — | _screenshot.scss:126-128 |
| Area selector handle | `.screenshot-ui-area-selector-handle` | — | bg white; box-shadow `0 1px 3px 2px shadow` | `999px` | size `24px` | — | _screenshot.scss:131-137 |
| Window-selector window border | `.screenshot-ui-window-selector-window-border` | — | border `6px transparent`; `:hover` darken(accent,15%); `:checked` accent + bg transparentize(accent,.8) | `32px` | 200ms | — | _screenshot.scss:154-185 |
| Tooltip | `.screenshot-ui-tooltip` (`%tooltip`) | body 11/400 centered | fg #fff; bg `rgba(0,0,0,.9)`; border 1px rgba(255,255,255,.1) | `999px` | `6px 12px`; y-offset `24px` | — | _screenshot.scss:199-203 |

### corner-ripple

Styles the hot-corner "activities ripple" animation shown when the pointer hits the top-left hot corner.

| Element | CSS class | fg / bg / border | Radius | Size | Source |
|---|---|---|---|---|---|
| Ripple box | `.ripple-box` | bg `rgba(255,255,255,0.2)`; halo box-shadow `0 0 2px 2px rgba(255,255,255,0.2)` | corner `52px` (curves one corner; `:rtl` mirrors) | `52px × 52px` (`$ripple_size` 50px + 2px) | _corner-ripple.scss:3-15; js layout.js:1189 |

## 2.5 Overview, apps, and the rest

### overview
Overview background dimming + the container hosting overview groups.

| Element | CSS class | fg / bg / border | Padding / spacing | Source |
|---|---|---|---|---|
| Secondary-monitor row | `.secondary-monitor-workspaces` | inherit | spacing 12px | _overview.scss:4 |
| Overview root group | `#overviewGroup` | bg `$system_base_color` #222226 | | _overview.scss:8 |

### app-grid
Icon-grid layout, app/folder tiles, folder popup, page indicators, system-action tiles.

| Element | CSS class (resolved @extend) | Font | fg / bg / border | Radius | Padding / spacing | Icon | Source |
|---|---|---|---|---|---|---|---|
| Grid layout | `.icon-grid` | — | — | — | row/col spacing 12px (max 36); page-pad 24px/18px | — | _app-grid.scss:7-16 |
| App icon tile | `.overview-tile` (`%tile`+flat dark tile_button) | inherit | fg #fff; bg transparent flat (from #222226) | 24px (`base*3`) | padding 12px; spacing 6px; centered; 100ms | — | _app-grid.scss:21-37 |
| App folder tile | `.app-folder` (raised tile_button) | inherit | fg #fff; bg `mix($system_fg_color,$system_base_color,9%)` **#353539** — raised, so unlike the flat app tile it has a resting fill | 24px + padding 12px (the class is `overview-tile app-folder`, `appDisplay.js:2288`) | icon = 2×2 of members at 0.4× (`appDisplay.js:2138-2162`) | — | _app-grid.scss:41 |
| Running-app dot | `.app-grid-running-dot` | — | bg #fff | 5px | 5×5px; offset-y 6px | — | _app-grid.scss:45-51 |
| Folder popup dialog | `.app-folder-dialog` | inherit | fg #fff; bg `$system_overlay_bg_color`; border 1px `$system_borders_color` | 64px (`$modal_radius*4`) | 720×720px; inset shadow; padding `0 1px`; its container pads the top by `$panel_height`, so it centers in the *work area* | — | _app-grid.scss:53-73 |
| Folder name (`%title_1`) | `.folder-name-label`/`.folder-name-entry` | 20/800 | system entry | | container padding `24px 36px`, `padding-bottom: 0` | — | _app-grid.scss:75-87 |
| Page indicator dot | `.page-indicator-icon` | — | bg #fff | 999px | dot 10×10px; 400ms | — | _app-grid.scss:120-132 |
| System-action icon (power) | `.system-action-icon` | — | fg #fff; bg white@10% | 999px | | 48px | _app-grid.scss:139-147 |
| Page-nav hint gradient | `.page-navigation-hint` | — | white@5%→transparent | 24px leading corner | | — | _app-grid.scss:150-170 |
| Page-nav arrow | `.page-navigation-arrow` | — | icon #fff; flat dark button | 999px | margin 6px; padding 18px | 24px | _app-grid.scss:172-185 |

### dash
Bottom/side dash: rounded backdrop, app icon buttons, running dot, separator, hover tooltip.

| Element | CSS class (resolved @extend) | fg / bg / border | Radius | Padding / spacing | Icon | Source |
|---|---|---|---|---|---|---|
| Dash container | `#dash` | — | — | padding-left/right 6px | — | _dash.scss:13-16 |
| Dash background | `.dash-background` | bg `$system_overlay_bg_color` | 28px (`modal 16 + dash_pad 12`) | padding 12px/10px | — | _dash.scss:19-30 |
| App icon button | `.overview-tile`/`.show-apps` (flat) | fg #fff; bg transparent flat | %tile 16px on `.overview-icon` | margin 0 2px; padding-bottom 12px | — | _dash.scss:49-69 |
| Running dot | `.app-grid-running-dot` | bg #fff | 5px | offset-y −12px | — | _dash.scss:72-79 |
| Separator | `.dash-separator` | bg `$system_borders_color` | — | width 1px; margin 4px | — | _dash.scss:83-98 |
| Tooltip (`%tooltip`) | `.dash-label` | fg #fff; bg black@90%; border 1px white@10% | 999px | padding 6px 12px; y-offset 8px | — | _dash.scss:103-106 |

### window-picker
Overview window-selector: caption tooltip, hover close button, rounded workspace backdrop.

| Element | CSS class (resolved @extend) | fg / bg / border | Radius | Padding | Icon | Source |
|---|---|---|---|---|---|---|
| Container | `.window-picker` | — | — | spacing 6px | — | _window-picker.scss:5-8 |
| Window caption (`%tooltip`) | `.window-caption` | fg #fff; bg black@90%; border 1px white@10% | 999px | padding 6px 12px | — | _window-picker.scss:24-26 |
| Close button | `.window-close` | fg #fff; bg lighten(system_bg,7%); border 2px transparent | 999px | padding 3px | 24px; box 32px | _window-picker.scss:29-54 |
| Workspace backdrop | `.workspace-background` | bg chroma-key transparent; shadow 0 4px 16px 4px | 30px | — | — | _window-picker.scss:56-61 |

### workspace-thumbnails
Vertical workspace-switcher strip.

| Element | CSS class | fg / bg / border | Radius | Size | Source |
|---|---|---|---|---|---|
| Strip | `.workspace-thumbnails` | — | — | width 32px; spacing/pad 6px | _workspace-thumbnails.scss:4-8 |
| Single thumbnail | `.workspace-thumbnail` | bg lighten(system_bg,10%); border 1px transparent | 4px | | _workspace-thumbnails.scss:9-18 |
| Active outline | `.workspace-thumbnail-indicator` | border 3px solid accent | 8px | | _workspace-thumbnails.scss:29-32 |

### search-results
Overview search: results panel, provider headings, list/grid tiles, "no results".

| Element | CSS class (resolved @extend) | Font | fg / bg / border | Radius | Padding / spacing | Source |
|---|---|---|---|---|---|---|
| Root | `#searchResults` | — | — | — | margin 0 4px | _search-results.scss:4-6 |
| Provider section | `.search-section` | | | | spacing 18px | _search-results.scss:13-22 |
| Section content box | `.search-section-content` | inherit | fg #fff; bg `$system_overlay_bg_color`; border 2px transparent | 36px (`modal*1.5`) | padding 12px; margin 0 12px | _search-results.scss:26-36 |
| "No results" (`%title_1`) | `.search-statustext` | 20/800 | fg system_fg@80% | | | _search-results.scss:48-51 |
| Grid result tile | `.grid-search-result` (=`.overview-tile`) | inherit | fg #fff; bg transparent flat | 24px | padding 12px; spacing 6px | _search-results.scss:58-60 |
| List result item | `.list-search-result` | | flat tile_button | 13.2px | spacing 6px | _search-results.scss:86-92 |
| List result desc | `.list-search-result-description` | | fg system_insensitive | | | _search-results.scss:104-110 |

### misc
Standalone overlays: select-area rubberband, user avatar, lightbox/flash, caps-lock, ws-switch backdrop, tile-drag preview.

| Element | CSS class | fg / bg / border | Radius | Icon | Source |
|---|---|---|---|---|---|
| Rubberband | `.select-area-rubberband` | bg accent@30%; border 1px accent | — | — | _misc.scss:2-5 |
| User icon | `.user-icon` | fg #fff; bg fg@5% | 999px | 64px | _misc.scss:8-19 |
| Lightbox / flashspot | `.lightbox`/`.flashspot` | bg black / bg white | — | — | _misc.scss:29-30 |
| Caps-lock warning (`%caption`) | `.caps-lock-warning-label` | 9/400; fg warning | — | — | _misc.scss:36-41 |
| WS-switch backdrop | `.workspace-animation` | bg `$system_bg_color` | — | — | _misc.scss:45-47 |
| Tile-drag preview | `.tile-preview` | bg accent@50%; border 1px accent | — | — | _misc.scss:50-53 |

### a11y
Accessibility overlays: mouse-location ripple, click-assist pie timer, magnifier border.

| Element | CSS class | fg / bg / border | Radius | Source |
|---|---|---|---|---|
| Pointer ripple | `.ripple-pointer-location` | bg lighten(accent@30%); halo shadow | 25px | _a11y.scss:2-8 |
| Pie timer | `.pie-timer` | border 3px accent; bg lighten(accent@40%) | — | _a11y.scss:11-17 |
| Magnifier region | `.magnifier-zoom-region` | border 2px solid accent (full-screen: 0) | — | _a11y.scss:20-24 |

### ibus-popup
IBus candidate popup: boxpointer bubble, candidate rows, index labels, page buttons.

| Element | CSS class (resolved @extend) | fg / bg / border | Radius | Padding | Icon | Source |
|---|---|---|---|---|---|---|
| Content bubble | `.candidate-popup-content` | bg `#36363a`; border 1px outer; shadow | 12px | padding/spacing 6px | — | _ibus-popup.scss:7-12 |
| Index label | `.candidate-index` | fg insensitive | — | padding-right 6px | — | _ibus-popup.scss:14-18 |
| Candidate row | `.candidate-box` | selected bg accent, fg #fff; hover bg hover | 8px | padding 6px 12px | — | _ibus-popup.scss:20-25 |
| Page button | `.candidate-page-button` | — | 8px | padding 6px | 16px | _ibus-popup.scss:33-40 |

### keyboard
On-screen keyboard: OSD backdrop, keys (char/default/latched), subkeys popup, emoji keys, suggestion bar.

| Element | CSS class (resolved @extend) | Font | fg / bg / border | Radius | Padding | Icon | Source |
|---|---|---|---|---|---|---|---|
| Root | `#keyboard` | — | bg `$osd_bg_color`; inset top border | — | — | — | _keyboard.scss:9-12 |
| Character key | `.keyboard-key` | ≈16pt bold | fg #fff; bg `#4d4d4d`; hover/active/checked | `to_em(8px)` | — | — | _keyboard.scss:31-43 |
| Default key | `.keyboard-key.default-key` | | fg #fff; bg `#363636`; latched bg accent | | border none | — | _keyboard.scss:45-56 |
| Key icon | `.keyboard-key StIcon` | | | | | 24px | _keyboard.scss:59 |
| Subkeys popup | `.keyboard-subkeys-boxpointer` | | bg osd; border 1px lighten; shadow | 22px | arrow-rise 10px | — | _keyboard.scss:63-79 |
| Emoji key (latched) | `.emoji-panel .keyboard-key:latched` | | border lighten(accent,5%); bg accent | | | — | _keyboard.scss:90-95 |
| Suggestion bar (`%title_4`) | `.word-suggestions` | 13/700 | fg #fff | | spacing 12px; padding 12px | — | _keyboard.scss:98-104 |

### login-lock
GDM login + screen-shield/lock: user list, prompt entries, auth-method list, session buttons, clock, notifications, parental shield.

| Element | CSS class (resolved @extend) | Font | fg / bg / border | Radius | Padding / spacing | Source |
|---|---|---|---|---|---|---|
| Prompt layout | `.login-dialog-prompt-layout` | — | fg #fff | — | width 25em; spacing 9px | _login-lock.scss:6-19 |
| Login prompt entry (`%system_entry`) | `.login-dialog-prompt-entry` | inherit | dark entry | 8px | padding 9px | _login-lock.scss:24-26 |
| Unlock prompt entry (`%lockscreen_entry`) | `.unlock-dialog .login-dialog-prompt-entry` | inherit | fg #fff; bg fg@10%; focus ring fg@40% | 8px | padding 9px | _login-lock.scss:216-220 |
| Session/a11y buttons (`.icon-button`+`%system_button`) | `.login-dialog-button.*` | — | dark system button | circular | padding `to_em(16px)` = 16px; so 48px across with a 16px glyph | _login-lock.scss:35-50 |
| User-list item | `.login-dialog-user-list-item` | — | dark button; `:logged-in` icon border accent | 16px | padding 9px; spacing 12px | _login-lock.scss:177-209 |
| Screen-shield backdrop | `.screen-shield-background` | — | bg black; shadow | — | | _login-lock.scss:228-231 |
| Lock clock time (`%numeric`) | `.unlock-dialog-clock-time` | 72pt/800 | fg #fff | — | | _login-lock.scss:242-246 |
| Lock clock date (`%title_1`) | `.unlock-dialog-clock-date` | 20/400 | fg #fff | — | | _login-lock.scss:248-251 |
| Notification card | `.message`/`.unlock-dialog-notification-source` | — | fg #fff; bg fg@10% | 16px | padding 12px 16px | _login-lock.scss:306-325 |
| Parental shield button (`%lockscreen_button`+`%title_4`) | `.parental-controls-shield-button` | 13/700 | fg #fff; bg fg@10% | 999px | padding 16px 44px | _login-lock.scss:262-284 |
| User widget label horiz (`%title_3`) | `.user-widget.horizontal .user-widget-label` | 15/700 | fg #fff | — | spacing 18px | _login-lock.scss:369-374 |

### looking-glass
Developer console: dialog + toolbar/tabs, property inspector, evaluator, window/extension/actor panes.

| Element | CSS class (resolved @extend) | Font | fg / bg / border | Radius | Padding | Source |
|---|---|---|---|---|---|---|
| Main dialog (`%osd_panel`) | `#LookingGlassDialog` | — | fg #fff; bg osd@98%; border 2px transparent; shadow | 16px | padding/spacing 6px | _looking-glass.scss:34-52 |
| Text entry (`%osd_entry`) | `.lg-dialog StEntry` | inherit | dark osd entry | 8px | min-height 22px | _looking-glass.scss:6-9 |
| Toolbar button (`%osd_button`) | `.lg-toolbar-button` | — | dark osd button | — | padding 6px 12px | _looking-glass.scss:60-65 |
| Notebook tab | `.notebook-tab` (`%osd_button_flat`) | — | transparent; selected active | — | padding 6px 12px | _looking-glass.scss:72-81 |
| Actor link (`%monospace`) | `.actor-link` | monospace | fg darken(osd_fg,20%); hover #fff | — | | _looking-glass.scss:19-25 |
| Window/extension card (`%card_common`) | `.lg-window`/`.lg-extension` | — | fg #fff; bg card; shadow | 12px | padding 12px | _looking-glass.scss:135-166 |
| Inspector title/name (`%heading`) | `.lg-obj-inspector-title`/`.lg-extension-name` | 11/700 | | | | _looking-glass.scss:97-170 |
| "No extensions" (`%title_4`) | `.lg-extensions-none` | 13/700 | fg osd_fg@50% | | | _looking-glass.scss:180-183 |

---

## 3. Port-status ledger

Component-level state of our reimplementation. `module` is under `src/`. Keep this current as
you port; the §2 tables stay pure GNOME reference.

**Panel status/indicator backlog:** the exhaustive gap inventory for the top panel — every missing
indicator, its GNOME 50.1 source, dependency, and a prioritized slice order — lives in
`docs/fork/panel-status-port.md`. Start there for panel-status work.

| GNOME component (§2) | our module | status | notes |
|---|---|---|---|
| design tokens §1 | `ui::pt_to_px` + constants across `ui/*` | 🟡 | accent palette + white accent-fg + dark bg match; pt→px unified via `ui::pt_to_px` (§1.3); the `%`-font scale + spacing/radii are not yet a shared token module |
| shared button/entry/switch/slider primitives | — | ⬜ | no reusable `%button` primitive yet; each UI draws bespoke rounded rects. The QS tile approximates `%button`/`%default_button` inline |
| **top panel** | `ui/panel.rs` | ✅ | see element map below |
| **quick settings** | `ui/quick_settings.rs` | 🟡 | tiles + system row + battery pill done; sliders, submenus, network/bt sub-menus, subtitles TODO. Element map below |
| calendar / date menu | `ui/calendar.rs` | 🟡 | month grid rendered; events/world-clocks/weather cards + today-button TODO |
| popup-menu / popovers | `ui/popover.rs` | 🟡 | boxpointer + modal-grab framework exists; `.popup-menu-item`/separator/submenu styling TODO |
| dialogs (modal-dialog) | `ui/end_session_dialog.rs`, `run_dialog.rs`, `exit_confirm_dialog.rs` | 🟡 | three dialogs drawn bespoke (not via a shared `.modal-dialog`); message-dialog, polkit, password-prompt TODO |
| notifications / message-list | `ui/config_error_notification.rs` | ⬜ | one-off error notice only; the banner + message-history system is unbuilt |
| OSD (volume/brightness/etc.) | — | ⬜ | TODO — deferred pending audio on the new VMM |
| switcher-popup (alt-tab) | `ui/mru.rs`, `ui/mru/` | ⚠️ | our MRU is **niri-origin**, not a port of GNOME's altTab; intentional divergence |
| workspace-switcher OSD | — | ⬜ | TODO |
| screenshot UI | `ui/screenshot_ui.rs` | 🟡 | panel chrome + capture button done; type toggles, tooltips, window/screen selectors partial |
| overview / app-grid / dash / window-picker / workspace-thumbnails / search-results | — | ⬜ | TODO (whole overview) |
| on-screen keyboard | — | ⬜ | TODO |
| login / lock (GDM, screen-shield) | — | ⬜ | TODO |
| looking-glass, ibus-popup, a11y overlays, corner-ripple | — | ⬜ | TODO |
| hotkey overlay | `ui/hotkey_overlay.rs` | ⚠️ | niri-origin (no GNOME equivalent) |

### 3.1 Top panel — element → constant (`ui/panel.rs`)

| GNOME element | our const | value | status |
|---|---|---|---|
| panel height (2.2em @11pt ≈ 32px) | `PANEL_HEIGHT` | 32.0 | ✅ |
| clock font (`.clock`, `panel_button` bold, 11pt) | `FONT_PX` + bold glyph run | 13.0, weight 700 | ✅ (px < GNOME nominal 14.7; see §1.3) |
| workspace dot (`$scalable_icon_size*0.5`) | dot-diameter const | 8.0, fully rounded | ✅ |
| status tray icons (network/battery) | via `system_status` model | 16px | ✅ |

### 3.2 Quick settings — element → constant (`ui/quick_settings.rs`)

| GNOME element | our const | value | status |
|---|---|---|---|
| menu radius (`$modal_radius*2.25` = 36px) | `MENU_RADIUS` | 36.0 | ✅ |
| menu bg | `MENU_BG` | `[0.12,0.12,0.12,1]` | ✅ (≈`$bg_color`) |
| tile size | `TILE_W`,`TILE_H` | 150×56 | 🟡 (GNOME `12em`×`48px`; close) |
| tile icon (`.quick-toggle-icon` 16px) | `TILE_ICON` | 16.0 | ✅ |
| tile title (`%heading` 11pt/700) | `LABEL_PX` + bold | 13.0, weight 700 | ✅ (px < nominal; §1.3) |
| tile-off bg (`%button`) | `TILE_OFF` | `[0.24,0.24,0.24,1]` | ✅ |
| tile-on fg (accent-fg #fff) | `FG_ON` | `[1,1,1,1]` | ✅ |
| tile-on bg (accent) | (drawn accent fill) | — | 🟡 (accent color not yet themed) |
| system button icon (`.icon-button` 16px) | `SYS_ICON` | 16.0 | ✅ |
| system button disc (icon+2×12px pad = 40px) | `SYS_HIT` | 40.0, fully rounded | ✅ |
| system button gap (`$base_padding*2`) | `SYS_GAP` | 12.0 | ✅ |
| battery pill | `PILL_W` | 96.0 | 🟡 (bespoke; GNOME PowerToggle differs) |
| subtitle (`%caption` 9pt) | — | — | ⬜ |
| sliders / submenus | — | — | ⬜ |
