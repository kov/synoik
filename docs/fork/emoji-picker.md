# Emoji picker

**Status: designed, nothing built.** A shell-owned emoji picker on `<Control><Alt>space`, macOS
style: it opens over whatever has focus, and picking inserts the character into that client without
the client ever losing focus.

**This is an addition, not a port.** GNOME has no shell-level emoji picker and no such binding —
GTK's `Ctrl+.` chooser is per-widget, inside the app. What we reuse from GNOME is the *delivery
mechanism*, which its on-screen keyboard already validates: `js/ui/keyboard.js` →
`Main.inputMethod.commit(str)`.

---

## Why it works: we are the input method

`src/input_method/` makes synoik the IM for every client, so `State::commit_text`
(`src/input_method/mod.rs:1103`) already sends arbitrary UTF-8 — ZWJ sequences and variation
selectors included — to the focused `zwp_text_input_v3`. The picker is a new caller of that
function plus a UI; no new protocol.

**Coverage, probed on this machine (2026-08-30), not recalled:**

| Client | text-input-v3 | Note |
| --- | --- | --- |
| GTK 3 / GTK 4 | yes | covers every GTK app and every VTE terminal |
| ghost | yes | `set_ime_allowed(true)` (`ghost-ui/src/lib.rs:2479`), handles `Ime::Commit` (`:7845`) |
| Firefox | yes | libxul logs "dispatching synthesized key events for Wayland text-input" |
| Chrome | yes | binds `zwp_text_input_manager_v3` (ozone) |
| kitty, alacritty | no | falls to the clipboard path below |
| Xwayland clients | no | text-input is a Wayland protocol; same fallback |

**There is no key-synthesis fallback, and GNOME does not have one either.** Mutter's virtual device
resolves a keyval only to a keycode already in the current group
(`pick_keycode_for_keyval_in_current_group_in_impl`, `meta-virtual-input-device-native.c:468`) and
warns `No keycode found for keyval` otherwise — so GNOME's own emoji→X11 path silently drops the
character. A wtype-style temporary keymap swap would work but makes every client reparse the keymap
mid-keystroke; rejected.

**Fallback: clipboard + OSD.** When nothing can receive text, picking sets the clipboard
(`clipboard.rs:202`) and shows an OSD saying so. Decided 2026-08-30.

## Placement

Anchor at the caret, fall back to the pointer. `zwp_text_input_v3.set_cursor_rectangle` is currently
discarded (`src/input_method/mod.rs:504`); store it, map surface-local → global through the window's
location. Treat a degenerate or absent rect (clients send `0,0,0,0`, or never send one) as no
anchor. The picker opens on the display owning the anchor.

## The grab: keys without focus

`text-input-v3` enter/leave rides `wl_keyboard` focus, and every shell-owned `KeyboardFocus` variant
returns `surface() == None` (`src/synoik.rs:1781`). So the picker **must not appear in
`update_keyboard_focus` at all**: the client keeps `wl_keyboard` focus, and the picker intercepts
keys in the input filter instead.

`FilterResult::Intercept` returns before `input_forward` (smithay `input/keyboard/mod.rs:971`), so an
intercepted key delivers neither the key *nor* the modifier update to the client. Three clauses keep
the client's view of the keyboard truthful:

1. **Presses are intercepted** — the picker owns them.
2. **Releases of keys the client already holds are forwarded.** A key held when the picker opens
   (autorepeating in a terminal) would otherwise repeat forever, since Wayland clients repeat
   themselves. `forwarded_pressed_keys` is `pub(crate)` in smithay, so snapshot
   `KeyboardHandle::pressed_keys()` minus `suppressed_keys` at open and forward releases for that
   set.
3. **Modifier keys are forwarded both directions**, so Ctrl/Alt never stick. The opening chord needs
   no special case: the client saw the Ctrl/Alt presses before the bind fired, and clause 3 delivers
   their releases.

Bindings that make sense over a picker (volume, media, brightness) still resolve, through the
existing `allowed_during_popup` filter shape (`src/input/mod.rs:1293`). A click on another window
closes the picker and then proceeds normally.

**The search entry does not use the IM.** Routing it through `im_offer_shell_key` would make it a
`ShellEntry`, and `sync_im_focus` (`mod.rs:578`) would move `ImFocus` off `Client` and reset the
engine — the very focus we are protecting. Plain `key_char` editing over `ui::text_edit::TextEdit`;
no dead keys in the search box, which costs nothing for ASCII emoji names.

**Where the bind is inert:** while locked (`is_locked()`), and while any modal `KeyboardFocus` owns
focus with no text target — switcher, screenshot UI, end-session. It stays live over the shell's own
entries (overview search, run dialog, folder/workspace rename), which work for free: `commit_text`
already routes `ImFocus::Shell` to `commit_into_shell_entry`.

## Data

A generated table, **vendored in-tree and never fetched at build time**, from Unicode's
`emoji-test.txt` (order, groups, names) plus CLDR annotations (search keywords). The fetch is a
manual `xtask` step, exactly as GNOME treats `data/update-osk-layouts.sh` — note GNOME's
`emoji.json` is generated and not in its tree, so there is nothing to copy.

Keywords are the point: "party", "thumbs", "joy" must all hit. Skin tone is one global preference
plus a per-emoji variant popover, as in `EmojiSelection` (`js/ui/keyboard.js:862`).

## Recents

Share GTK's history: `org.gtk.gtk4.Settings.EmojiChooser recently-used-emoji`, so our picker and
every GTK app's `Ctrl+.` show the same emoji. The schema type `a((aussasasu)u)` is authoritative for
shape only — field semantics are in `gtkemojichooser.c`, which is not on this machine. **Parse
defensively, and only write back once a read→write round-trip of GTK's own data is byte-identical.**
Until that is verified: import their recents, write ours to our own key.

## Keybinding

A new `emoji-picker` key in `org.synoik.keybindings` (`resources/schemas/`, mirrored by
`adopted_synoik_keybindings`, `src/gnome.rs:2128`), default `<Control><Alt>space`;
`synoik_accels_do_not_collide_with_gnome` proves it takes no GNOME chord. It shadows emacs'
`mark-sexp` (`C-M-SPC`) — compositor binds win, and the key is rebindable. Add it to the hotkey
overlay (`src/ui/hotkey_overlay.rs`).

## Slices

1. **Data + generator.** `xtask` producing the vendored table; search index and its unit tests.
2. **Insertion seam.** Public wrapper over `commit_text`, clipboard+OSD fallback, and a
   `debug-emoji-insert <char>` IPC action. Conformance test: the test client asserts
   `TextInputEvent::CommitString` (`src/tests/client.rs:1730`), and a second asserts the clipboard
   path when no text input is active.
3. **Caret anchor.** Store `CursorRectangle`, map to global, pointer fallback.
4. **The grab.** Open/close, the three key clauses above, no `KeyboardFocus` participation. Test that
   the client keeps focus, receives no printable keys, and is left with no stuck key or modifier.
5. **UI.** Grid, category rail, search entry, hover/keyboard selection, tone popover; chrome from
   `docs/fork/gnome-style-reference.md`.
6. **Recents**, per the round-trip rule above.
7. **Keybinding** + hotkey overlay.
