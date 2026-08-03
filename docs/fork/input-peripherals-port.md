# Input devices: `org.gnome.desktop.peripherals`

Status: **landed** for touchpad, mouse, trackball, pointingstick and keyboard repeat/numlock.
Tablet and touchscreen are deferred (below).

Per the fork tenet, GNOME's way replaces niri's: device settings used to come from the config
file's `input {}` block and now come from the schemas GNOME Settings writes, so
**Settings → Mouse & Touchpad** is how a device is configured. The config file is gone entirely
(see `RUNNING.md`), so this is not a second way to do it — it is the only way.

Code: `src/input/peripherals.rs` (the model), `src/gnome.rs` (`Stores` opens the schemas and
`read()` builds it), `State::apply_peripherals` in `src/niri.rs` (the hand-off).
Reference: mutter `src/backends/meta-input-settings.c` + `src/backends/native/meta-input-settings-native.c`.

## Shape

`Peripherals` produces the **existing `niri_config` device structs**, not a new model. That is
the point: `apply_libinput_settings` already knows how to push a `niri_config::Touchpad` onto a
libinput device, the scroll-factor lookups and the key-repeat timer read `config.input` where
they always did, and a hotplugged device is configured by the same code path as before. The
GSettings read replaces only where the values *come from*.

`Config::default()`'s input block is now GNOME's schema defaults, key for key, so a box with no
GNOME schemas installed behaves like a GNOME session nobody has configured.
`peripherals_defaults_match_a_pristine_gnome_store` pins that by loading five untouched stores
and asserting the result equals `Peripherals::default()` — it caught `numlock` on the first run
(the shipped config file had it on; GNOME's default is off).

## Where the shapes do not line up

| GNOME | ours | note |
|---|---|---|
| touchpad `send-events` (3-way enum) | `off` + `disabled_on_external_mouse` | one enum, two booleans |
| touchpad `edge-scrolling-enabled` + `two-finger-scrolling-enabled` | `scroll_method` | two booleans, one enum; two-finger wins when both are on (mutter's rule, `meta-input-settings.c:806-808`) |
| touchpad `left-handed` = `mouse` | `left_handed` | defers to the *mouse* key, which is why the mouse is read first |
| keyboard `repeat` + `repeat-interval` (ms) | `repeat_rate` (Hz) | `repeat = false` becomes rate 0, the only spelling of "no repeat" downstream |
| keyboard `remember-numlock-state` + `numlock-state` | `numlock` | the state only counts when GNOME was asked to remember it |
| trackball `scroll-wheel-emulation-button` (X11 button no.) | `scroll_button` (evdev code) | 0 means no scrolling at all, not "device default" |
| `pointingstick` | `trackpoint` | same device, different name |

**One real divergence.** Mutter checks per device whether two-finger scrolling is *available*
before preferring it; this model is built once for all devices, so a touchpad without it and
with both keys set gets `TwoFinger` asked for, libinput refuses, and the device keeps its
default method. Fixing it means moving the collapse into `apply_libinput_settings`, where the
device is in hand.

## Gaps and deferred

- `click-method='none'` — libinput has it, the `input` crate's `ClickMethod` does not. Warns and
  falls back to the device default.
- touchpad `disable-while-typing-timeout`, mouse `double-click` / `drag-threshold` — no libinput
  knob; the last two are toolkit-level in GNOME and never reach the compositor.
- `dwtp` — ours, no GNOME key. Stays off.
- **Tablet and touchscreen** (`peripherals.tablet`, `.tablet.stylus`, `.tablet.pad-button`,
  `.touchscreen`): deferred. These are *relocatable* per-device schemas keyed by vendor/product,
  and they carry output mapping, calibration and per-button actions — a port with its own
  device-identity and display-mapping problem, not a rider on this one. `config.input.tablet` and
  `.touch` still exist and still hold compiled-in defaults.
- `input.warp_mouse_to_focus`, `focus_follows_mouse`, `disable_power_key_handling`,
  `workspace_auto_back_and_forth`, `mod_key`: not device settings; unreachable compiled-in
  defaults for now.
