# UI Dump — ask the running shell for its box model

A development extension for the **reference** GNOME session, not for our compositor. It dumps the
live actor tree as JSON: each actor's allocation *plus* what `StThemeNode` actually resolved for
padding, margin, border width, radius, spacing, icon size, colors and font.

**Why.** Porting a widget from the SCSS means reading a cascade and hoping you stacked the
containers the way St does. A screenshot tells you where an edge landed, never which container put
it there — and a wrong model looks identical to a right one until some other widget disagrees. This
turns "derive, then hope" into "read the answer".

It came out of a real miss: the message-list card had `.message-header-content`'s `padding-bottom`
collapsed into `.message-box`'s padding, and `.message-icon`'s `margin` collapsed into
`.message-box`'s `spacing` — two stacking pairs treated as one each, so three separate insets were
6px short and the fourth (which has no such pair) looked fine. Screenshot measurement showed the
edges were wrong; only reading the cascade explained why.

## Install (reference session only)

**Installing needs a shell restart, i.e. a logout on Wayland.** `ExtensionManager._loadExtensions()`
enumerates the extension directories exactly once, guarded by `_initializationPromise`
(`js/ui/extensionSystem.js:687,750`); the only `reloadExtension` triggers are session-mode changes
and the extensions-website update path. There is **no filesystem monitor for newly dropped
extensions**, so dropping a directory in and calling `gnome-extensions enable` cannot work — the
shell has never heard of the uuid. The same applies to *editing* the code: every change needs
another logout.

Copy it in — a **real directory, not a symlink** (the scan wants a directory) — and pre-enable it,
so it is live the moment you log back in:

```sh
cp -r ~/Projects/gnome-shell-rs/tools/gnome-ui-dump \
      ~/.local/share/gnome-shell/extensions/ui-dump@gnome-shell-rs
# `gnome-extensions enable` fails before the shell knows the uuid; write the setting directly.
gsettings get org.gnome.shell enabled-extensions   # then append 'ui-dump@gnome-shell-rs'
```

Then log out and back in. Check it is up:

```sh
busctl --user introspect org.gnome.Shell.Extensions.UiDump /org/gnome/Shell/Extensions/UiDump
```

## Use

The method you usually want is `DumpClass` — it dumps every subtree whose root carries a style
class, along with each match's **ancestry**, which is where a nested-padding inset actually comes
from.

```sh
# With the calendar popover open in another workspace/monitor:
gdbus call --session -d org.gnome.Shell.Extensions.UiDump \
  -o /org/gnome/Shell/Extensions/UiDump \
  -m org.gnome.Shell.Extensions.UiDump.DumpClass "message" /tmp/message.json

# Something that closes when it loses focus? Schedule it, then open the UI by hand:
gdbus call --session -d org.gnome.Shell.Extensions.UiDump \
  -o /org/gnome/Shell/Extensions/UiDump \
  -m org.gnome.Shell.Extensions.UiDump.DumpClassAfter "message" /tmp/message.json 8
# ...open the date menu; the result line goes to the journal:
journalctl --user -b -g "\[ui-dump\]" -n 5
```

Also: `DumpName "calendarArea"` (for `#id` selectors) and `DumpAll` (the whole stage — large;
prefer the targeted ones).

## Reading it

Per actor: `type`, `name`, `style_class`, `inline_style`, `abs` (stage coordinates), `size`,
and `theme` with

- `padding`, `margin`, `border_width` as **`[top, right, bottom, left]`** (CSS shorthand order),
- `border_radius` as `[top-left, top-right, bottom-right, bottom-left]`,
- `spacing` — the theme length St hands the layout manager. **This is the one that catches people:**
  a box's `spacing` and a child's own `margin` both apply, so a gap is often the sum of two values
  that live in different rules.
- `icon_size`, `min_width`/`min_height`, `width`/`height`, `background_color`, `color`, `font`.

Colors are emitted as `{raw: [r,g,b,a], string}` because Cogl has used both 0-255 integers and 0-1
floats across versions — read the raw numbers rather than trusting a guess.

An inset is usually the sum down the `ancestry` chain: for a `.message`, `.popup-menu-content`
padding (6) + `#calendarArea` padding (4) = the 10px the first card sits in from the popover border.

## Caveats

- **Read-only**, but it is still code in the reference session: `disable()` drops the D-Bus name and
  any pending timeout.
- Values are **logical px at the session's scale**; a fractional-scale session reports accordingly.
- `get_theme_node()` fails on an unallocated actor, so `theme` can be `null` for something not yet
  laid out. Open the UI first.
- Tree walks are depth-capped (40) so a stray `DumpAll` cannot stall the shell.
