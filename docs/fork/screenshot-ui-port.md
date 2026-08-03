# The screenshot UI — porting GNOME's control panel

Status: **2026-08-03, scoping.** What we have is niri's area picker wearing GNOME's OSD chrome:
the shade, the selection rectangle, a capture button, and a panel that says *"Press Space to save
the screenshot"*. GNOME's panel is a row of real controls, and per the fork tenet its way replaces
niri's — the help text goes.

Reference: `js/ui/screenshot.js` (50.3) and `_screenshot.scss`. The visual spec is already cached
and cited in `docs/fork/gnome-style-reference.md` §screenshot (every class, colour, radius and
padding below comes from that table — re-grep the SCSS only if a row looks wrong).

## What GNOME builds, in construction order

Child order comes from the JS, never from what looks right (`screenshot.js:1226-1430`):

```
ScreenshotUI
├── _areaSelector            shade quadrants, selection outline, 4 corner handles
├── _primaryMonitorBin
│   ├── _panel               vertical, y_align END, %osd_panel
│   │   ├── _typeButtonContainer     [ Selection | Screen | Window ]   spacing 12, homogeneous
│   │   └── _bottomRowContainer      (BinLayout — all three overlap, aligned)
│   │       ├── _shotCastContainer   x_align START   [ shot | cast ]
│   │       ├── _captureButton       centred
│   │       └── _showPointerButtonContainer  x_align END  [ show-pointer ]
│   └── _closeButton         constrained to the panel's top-left, outside it
└── Tooltips                 added to the ROOT, so they draw above the panel
```

Two details that are easy to get backwards: the bottom row is a **BinLayout**, so the capture
button is centred on the panel rather than placed after the shot/cast pair; and the tooltips are
children of the root widget, not of the buttons they describe.

## What we have today

`src/ui/screenshot_ui.rs` (~1630 lines). Area selection on a single output, drag/move/resize by
pointer and by keyboard, the shade and selection chrome, a CPU-composed capture button, and
`generate_panel` — the two help lines with keycap patches. `P` toggles the pointer; `Space` saves.
No type buttons, no shot/cast control, no close button, no tooltips, no window selector.

## Slices

1. **The panel becomes GNOME's, with Selection and Screen.** Replace the help lines with the type
   row and the bottom row. `Selection` is what we already do; `Screen` selects the whole output.
   The show-pointer state becomes a real button (keeping `P` as its accelerator), and the close
   button lands. `Window` is built but hidden until slice 2 — GNOME hides `_castButton` the same
   way until screencasting is available, so a hidden-until-supported control is in keeping.
2. **Window mode.** The window selector: per-window borders with hover and checked states
   (`.screenshot-ui-window-selector-window-border`, accent-tinted), and capture of the chosen
   window. `ScreenshotWindow` already exists on the bus, so this is selection UI over an existing
   capture.
3. **Cast mode.** The shot/cast segmented control wired to our recorder, plus the capture button's
   `:cast` state (inner circle goes `$red_4`). This is the real trigger GNOME uses for the panel's
   recording indicator — see `docs/fork/panel-status-port.md` R1, which is currently driven by a
   direct `RecordArea` call for want of this.
4. **Tooltips.** `%tooltip`, 24px below the control, on the root so they draw above the panel.

## Toolkit first

Two controls here appear more than once and must land as `widget::` types rather than as drawing
inside `screenshot_ui.rs`:

- **Icon-over-label button** (`IconLabelButton`, 32px icon above a 9pt caption) — the three type
  buttons, and the same shape appears elsewhere in the shell.
- **Segmented toggle** (`.screenshot-ui-shot-cast-container`: a pill of translucent white holding
  buttons whose `:checked` state inverts to white-on-dark) — shot/cast here, and the same control
  shows up in the quick-settings and calendar surfaces.

## Icons

`preview-close-symbolic` is already vendored in `resources/icons/`. Needed from the reference
checkout (`gnome-shell/data/icons/scalable/actions/`, GPL, same practice as the icons we already
carry): `screenshot-ui-area-symbolic`, `screenshot-ui-display-symbolic`,
`screenshot-ui-window-symbolic`, `screenshot-ui-show-pointer-symbolic`. The shot/cast buttons use
`camera-photo-symbolic` and `camera-web-symbolic` from the icon theme.

## Cover

The corpus can open the picker only through the renderer (it has to freeze the screen first), so
the Vulkan tests in `src/tests/vulkan_render.rs` are where this is pinned —
`vulkan_screenshot_ui_draws_the_help_panel` is the test that has to change shape first, since the
help panel it asserts on is what slice 1 deletes.
