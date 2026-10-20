# Boot splash: the system compositor takes over from plymouth

> Status: **phase 1 built** (steps 1-4 of §5): the outer answers plymouthd's protocol, draws the
> splash, takes the screen over at boot and keeps it through shutdown and offline updates
> (`synoik-splash/src/{plymouth,layout}.rs`, `src/system_compositor/outer/boot.rs`).
> **Phase 2 built** (§7): the splash in the initramfs, in place of plymouth there
> (`synoik-splash/src/main.rs`), validated on the harness VM.
> **Password prompts built** (step 5, §4.2) in both drawers and validated: a LUKS root on Debian
> (§7.6), and on the harness a volume asked in the initramfs, one asked in the real root before
> the handover, and a prompt up across the handover, answered by the outer. **Emergency and
> rescue** (step 6, §2.4) validated on the harness, a thrice-failed passphrase included. **initramfs-tools** (Debian, §7.6) validated
> on a Debian VM with a LUKS root: prompt, unlock, handover and greeter, the splash on screen
> throughout; an offline update there too.

References: plymouth **24.004.60** (`~/Projects/plymouth`, Fedora 44's version; Fedora's patches
touch neither the protocol nor the layout math, only assets and HiDPI thresholds), systemd v259,
PackageKit 1.4.0 `client/pk-offline-update.c`, dnf5 5.4.6.0 `dnf5/commands/offline/offline.cpp`,
util-linux 2.41.5 `lib/plymouth-ctrl.c`, gdm 51.0 `daemon/gdm-manager.c`. Paths below are in the
plymouth checkout unless prefixed.

## 1. Decisions

- **Real root first, then the initramfs.** Phase 1: plymouth keeps the initramfs; the outer takes
  over after switch_root, with no visible change, and keeps the screen through shutdown. Phase 2
  (§7): a small splash program replaces plymouth in the initramfs, and the outer takes over from it
  by a restart of the same unit.
- **The outer speaks plymouthd's protocol.** Every tool that talks to plymouth today — systemd's
  password agent and unit-status updates, PackageKit and dnf5 offline updates, dracut, util-linux,
  battery-check, FIDO2 unlock — keeps working unchanged (§3).
- **The look is Fedora's `bgrt` theme, drawn natively** from the installed theme's own assets (§4),
  so the outer can crossfade from it into the greeter or session, and back into it at shutdown.

## 2. Handover and units

### 2.1 Boot: deactivate, modeset, quit

GDM's order (`gdm-manager.c:153-198`), not today's quit-first:

1. `ExecStartPre=+-plymouth deactivate` — plymouthd finishes, drops master (`main.c:1303-1317`) and
   **keeps its fd**, so its last frame stays scanned out. The ACK comes after the drop.
2. The outer opens the card (implicit master, as now) and modesets **its own replica of that
   frame** (§4): the same pixels, so the handover is invisible.
3. The outer sends `READY=1` and starts binding `\0/org/freedesktop/plymouthd` from its event
   loop, retrying `EADDRINUSE` on a timer — never blocking: the name frees only once step 4 has
   run, and step 4 runs only after `READY=1` (`Type=notify` holds `ExecStartPost=` until then).
4. `ExecStartPost=+-plymouth quit --retain-splash` — plymouthd exits; its framebuffer is no longer
   on any plane, so closing its fd changes nothing on screen, and ours keeps the device open
   (no `drm_lastclose`, no fbcon). It ACKs `Q` before freeing its server (`main.c:1280-1283` vs
   `:2573`), so the name stays taken for a moment after the command returns — the retry of step 3.
5. Bound, the outer writes `/run/plymouth/pid`. plymouthd created `/run/plymouth` root-owned and
   unlinks the pid file on quit, so the installer's tmpfiles entry hands the directory to
   `synoik-system` (`z`, applied by `systemd-tmpfiles-setup.service` before the outer starts), and
   the file is written only after the bind.

Measured on the harness VM (2026-10-14, plymouth 24.004.60, DRM plane state read from debugfs at
each step and every presented frame captured on the host): after `deactivate` plymouthd's
framebuffer is still on the primary plane; the outer's first frame replaces it directly; `quit`
changes nothing on screen. The quit-first order leaves the plane empty, with the CRTC lit, for
110-240 ms before the outer's first frame: a black flash. The frame `deactivate` leaves has the
spinner in it, mid-turn, so the outer's first frame is the splash with its spinner at frame 0: the
watermark matches plymouth's to within 1/255 per channel, and the spinner's phase jumps once, as
plymouth's own is not knowable.

The client side is root-only by plymouth's design (`SO_PEERCRED` uid 0 or NAK,
`ply-boot-server.c:265-285`), so steps 1 and 3 run as `+`; the outer itself stays unprivileged and
applies the same check to its clients.

Prompts are dead between step 1 (plymouthd releases the keyboards) and step 5; a prompt pending
in plymouthd when it quits is lost (the agent sees EOF, and does not ask again, §7.2). This path is
the fallback for an initramfs that still has plymouth; with the initramfs splash, real-root volumes
are unlocked before the handover, through the splash (§7.2).

The outer keeps the listener it binds in the unit's fd store, so a restart (`Restart=always`)
adopts the name rather than binding it again. Steps 1 and 4 run only while `/run/plymouth/pid`
names a `plymouthd`: after a restart the name is the outer's own, its listener in the store with
nobody accepting until the outer is up, and a `deactivate` sent there would wait forever.

### 2.2 Unit changes

| Unit | Change | Why |
|---|---|---|
| `synoik-system.service` | `DefaultDependencies=no`, `IgnoreOnIsolate=yes`, `Slice=-.slice`, `SurviveFinalKillSignal=yes`, and no `Requires=` on any mount (§2.3); started as early after switch_root as its files allow (§2.2.1); `WantedBy=` both `graphical.target` and `system-update.target`, with `Before=system-update-pre.target` | §2.1; offline updates; §2.3 |
| `plymouth-quit-wait.service` | drop-in `ConditionPathExists=/etc/initrd-release`: runs in the initramfs only | `plymouth --wait` sends nothing and returns only when the daemon closes it, with `TimeoutSec=0` before `multi-user.target`. The outer closes idle connections when the boot splash ends, but quit-wait may connect after that. |
| `plymouth-start.service` | the same drop-in | If the initramfs had no plymouthd, a fresh one would race the outer for the card. A condition rather than a mask or an emptied `ExecStart=`, so a drop-in that reaches the initramfs image changes nothing there. |
| `plymouth-{reboot,poweroff,halt,kexec}.service` | `ExecStart=` replaced by `plymouth change-mode --reboot\|--shutdown` (a oneshot) | plymouth passes the shutdown mode only as a daemon argument (`--mode=`), never over the protocol. |
| `plymouth-quit.service` | stays `Conflicts=` | as now |
| `plymouth-switch-root-initramfs.service` | unchanged | its `R` is ACKed and ignored |

The installer writes these; `--use-gdm` removes them.

#### 2.2.1 What replaces the default dependencies

`DefaultDependencies=no` drops `sysinit.target`/`basic.target` and the shutdown conflict, so the
unit names what it needs instead: `After=systemd-journald.socket systemd-tmpfiles-setup.service
systemd-sysusers.service systemd-udev-trigger.service`, and for now `dbus.socket
systemd-logind.service` too: the outer follows logind's seats and its `PrepareForShutdown`. Units
by name, not `After=basic.target`: in an update boot basic.target was still active from the
initramfs at switch_root, and the outer started there, before the real root's tmpfiles had handed
it `/run/plymouth` (measured 2026-10-14). The cache dir is the installer's tmpfiles entry, named to
Mesa through `XDG_CACHE_HOME`, not `CacheDirectory=`, which implies
`RequiresMountsFor=/var/cache` (§2.3); ordered after those units, its mounts are up, so the
`WantsMountsFor=` it would otherwise need is not there. The card has no systemd device
unit on Fedora (udev does not tag DRM cards for systemd), so the outer waits for it itself, from
its udev monitor, when it is not there yet; at switch_root it normally is (virtio-gpu loads in the
initramfs at ≈1.1 s on jabuticaba).

Real-root volumes are not the outer's to unlock: their prompts are answered by whichever splash
the initramfs started, still on screen, since the outer takes over only after them (§7.2).

#### 2.2.2 fbcon stays off, the fbdev client stays

**The initramfs splash needs the kernel's fbdev client.** A connector reads `unknown` until
something probes it; the fbdev client probes at driver load, and its change uevent is what
plymouthd waits for. Without it (`drm_kms_helper.fbdev_emulation=0`) nothing probes before the
outer and plymouthd shows nothing (measured on the harness VM, 2026-10-14). So fbcon is kept off
the outer's card with `fbcon=map:9` instead, which leaves the client in place
(`system-compositor.md` S6).

### 2.3 Shutdown and offline updates

- **The outer keeps the screen until power-off.** Today it is stopped for shutdown while systemd's
  fd store keeps master, so the shutdown plymouthd cannot draw either: nothing shows the shutdown
  splash. Four things each stop it on the way down, and each has its setting:
  `shutdown.target`'s conflict (`DefaultDependencies=no`); `systemctl poweroff|reboot` isolating
  its target (`IgnoreOnIsolate=yes`, as plymouth-start has); a `Requires=` on a mount that is
  unmounted for shutdown — `CacheDirectory=` adds one on `/var` (§2.2.1); and systemd-shutdown's
  final kill (`SurviveFinalKillSignal=yes`; plymouth's `@` argv mark protects only root processes,
  `systemd src/shared/killall.c:91-100`). The unit also leaves `system.slice` (`Slice=-.slice`),
  which shutdown stops; that one was not tested on its own.
  Measured on the harness VM (2026-10-14) with the installed unit, from a systemd-shutdown hook
  writing to the serial console after the final kill: the outer, still the process the boot
  started, alive and its framebuffer on the plane at "Power down", the session faded into the
  turning splash. Without the mount fix, systemd stopped it 2 s before the end.
- On `PrepareForShutdown` the outer crossfades from the session to the shutdown splash
  (it fades to black today) and spins until power-off. dracut's late `H` freezes the last frame.
  The mode (`C reboot|shutdown` from the rewritten units) arrives only after the display manager
  stops, long after `PrepareForShutdown`; the fade does not wait for it, because bgrt's boot,
  shutdown and reboot screens are identical (§4.1). Only an update's end changes the screen.
- **Offline updates** (`system-update.target`) start the outer too, and PackageKit and dnf5 drive
  its update screen through `C`, `u` and `M` (§3.3). The unit's last `ExecStartPost=` waits for the
  pid file, so the update starts only once the outer answers. Measured with a dnf5 offline
  reinstall on the harness (permissive, §6): "Installing Updates…" with the bar at 0, 47 and 100 %,
  then the reboot screen with dnf5's closing message under the spinner. `scripts/harness/update.sh`
  records such an update boot end to end.
- Open: the outer keeps files on the root filesystem mapped, so dracut's shutdown can only remount
  it read-only. A binary replaced by the update and still mapped may block even that. The harness
  printed no failure at the console's log level; an update replacing the outer's own libraries is
  untested.

### 2.4 No console to hand back

`plymouth quit` *without* `--retain-splash` comes from emergency.service, rescue.service,
dracut-emergency and sulogin: "give the text console back". With fbcon kept off the outer's card
(§2.2.2) there is none to give: the splash ends as with `--retain-splash`, and the emergency shell
is reached on the serial console or over ssh — S6's stated cost. Measured on the harness:

- **In the initramfs** (`rd.break`), dracut-emergency quits the splash through plymouth's client,
  which the image carries for it (§7.4): the screen keeps the background and watermark, the shell
  answers on the serial console, and the VT's keyboard is untouched (keys typed reach tty1). Once
  the shell exits, the boot goes on and the handover follows as usual.
- **In the real root** (`systemctl emergency`, `systemctl rescue`), the outer has no splash to end
  and acknowledges the unit's `plymouth --wait quit` at once. It stays up (`IgnoreOnIsolate=`)
  while the isolate stops the login daemon and every session with it, so the screen goes to the
  outer's empty background, with no session's content left on it. Leaving the shell brings the
  greeter back on the same outer.

## 3. The protocol

### 3.1 Transport

- Abstract `SOCK_STREAM` socket `"\0/org/freedesktop/plymouthd"`, trimmed address
  (`ply-boot-protocol.h:25`, `libply/ply-utils.c:132-170`). No `PrivateNetwork=` on the unit:
  abstract names live in the network namespace.
- Requests: `cmd 0x00`, or `cmd 0x02 L arg 0x00` with `L = strlen(arg) + 1` as a u8
  (`ply-boot-client.c:362-389`). `L == 0` (a 255-byte argument wrapped) is an empty argument.
  Several requests per connection, several per `write()` (systemd sends `M…` and `A` together):
  a stream parser.
- Replies: ACK `0x06`, NAK `0x15`, ANSWER `0x02` + u32 LE length + bytes, MULTIPLE_ANSWERS `0x09`
  + u32 LE length + NUL-terminated answers, NO_ANSWER `0x05` (`ply-boot-protocol.h:51-55`).
  In request order per connection, later requests queued behind a blocking one. `MSG_NOSIGNAL`;
  fire-and-forget clients close before reading.
- Missing arguments never crash: util-linux sends `Q 00` with none (plymouthd dereferences NULL).

### 3.2 Commands

| Cmd | Meaning | Outer |
|---|---|---|
| `P` | ping | ACK at once — util-linux gives up after 1 s, and agetty *quits* plymouth when the ping is late |
| `$` / `H` | show / hide splash | show; hide freezes the last frame |
| `C` | change mode: `boot-up`, `shutdown`, `reboot`, `updates`, `system-upgrade`, `firmware-upgrade`, `system-reset` | switch screen (§4.3), ACKed once the screen shows it (at most 1 s); unknown modes ACKed and ignored |
| `u` | update progress 0-100 | progress bar |
| `M` / `m` | show / hide message (exact match) | message list; an empty `M` clears it (systemd's FIDO2 code relies on that) |
| `*` | ask for password, prompt optional | the password dialog (§4.2); ANSWER on Enter. Never NO_ANSWER while a splash is up: systemd's agent takes it as "no UI", leaves the request and waits for the next one, and the console agent stands down for `/run/plymouth/pid`, so the asking unit would wait unseen (crypttab's default timeout is none) |
| `c` | cached passwords | NO_ANSWER — **never NAK**: systemd's agent turns anything but ANSWER/NO_ANSWER into -EIO |
| `Q` | quit, 1-byte retain flag | ends the boot splash (ACK when done); without retain, §2.4 |
| `U`, `A`/`a`, `R`, `S`, `!`, `D`, `r`, `l` | status, pause/unpause progress, newroot, sysinit, error, deactivate, reactivate, reload | ACK, no effect (`R` must never chroot) |
| `W`, `K` | question, keystroke watch | NO_ANSWER |
| `L` | ignore keystroke | ACK |
| `V` | has active VT | NAK |
| other | | NAK |

Ctrl+C and Ctrl+D clear the entry rather than cancel: plymouthd answers `"\x03"`, which systemd's
agent would take as the password, and NO_ANSWER would leave the request unanswered for good (above).
A dialog with no keyboard to type on stays up, for the same reason. Answers are not cached.

### 3.3 Callers on Fedora 44

- **systemd:** pid 1 sends `U <unit>` per unit started; `systemd-tty-ask-password-agent --plymouth`
  sends `c` then `*` (gated on `/run/plymouth/pid`); battery-check `C`+`M`; FIDO2 `M`+`A`, clear
  with `M ""`+`a`; emergency/rescue `plymouth --wait quit`.
- **PackageKit** `pk-offline-update`: `change-mode --updates|--system-upgrade`, `display-message`,
  `system-update --progress=N` (and "Installing Updates — N%" messages past 10 %), then
  `change-mode --reboot|--shutdown`. Exit codes ignored.
- **dnf5** `offline _execute`: `system-update --progress`, `display-message "[n/m] Upgrading …"`,
  `change-mode --updates|--system-upgrade`; **stops calling plymouth after its first failure**, so
  an early NAK silences it for the rest of the update.
- **util-linux** agetty/sulogin: `P 00`, `Q 00`.
- **dracut** (systemd initramfs): `H` from shutdown.sh; `quit` from dracut-emergency.

## 4. The splash

### 4.1 Boot, shutdown and reboot

What `two-step` draws for `bgrt` (`src/plugins/splash/two-step/plugin.c`,
`/usr/share/plymouth/themes/bgrt/bgrt.plymouth`, inheriting `spinner`), per display, in logical
pixels for a display of W×H:

- **Background:** black, and the firmware's logo where the kernel exports an ACPI BGRT table
  (`/sys/firmware/acpi/bgrt`; `synoik-splash/src/bgrt.rs`, a 24-bit BMP as plymouth reads it).
  Limina publishes one only on its ACPI machine (`limina --acpi`); a device-tree boot has none.
  Placement is two-step's `view_set_bgrt_background` (`layout.rs` `place_firmware`): centred
  horizontally with its centre at 38.2 % of the height, unless the firmware's x offset centres it
  on the *panel*, in which case its offsets hold there and other displays shift by half the extra
  width and 38.2 % of the extra height. The panel is the last built-in (LVDS, eDP, DSI) display,
  else the first in connector order (the drm renderer's rule), so on Limina at 1280×800 the logo
  stays at the firmware's 512×272. A logo for a rotated panel (status orientation bits) is left
  out, as two-step leaves it out on an upright one; the splash never rotates a panel. Shown in
  every mode and in the retained frame — bgrt leaves it out of the update modes, the splash keeps
  it (§4.3) — and cleared under a password dialog (`DialogClearsFirmwareBackground`). Both drawers read the table when they load
  the theme.
- **Spinner:** the theme's 30 `throbber-*.png` frames (the installed set; Fedora replaced the
  checkout's), 32×32, one turn per 2 s, top-left at (0.5W−16, 0.7H−16).
- **Watermark:** `watermark.png` (149×43, the same file as `/usr/share/pixmaps/fedora-gdm-logo.png`),
  top-left at (0.5W−74.5, 0.96H−41.28), in every state.
- **Messages:** one line, the latest `M`, centred, 10 px under the spinner. two-step has no
  `hide_message`, so `m` does nothing there (systemd clears with an empty `M` for that reason);
  the outer hides the line when `m` names it, which is what the request asks for.
- **No animation within the splash but a mode change's.** A client's `quit` stops it at background +
  watermark, plymouth's retained frame. The fades are the outer's: from the splash as it stands,
  spinner and all, to the first session frame, and the motion between the boot and the update
  screens (§4.3).

### 4.2 Password dialog

two-step's `view_show_prompt` (`synoik-splash/src/layout.rs` `lay_dialog`): a 35×34 lock beside
a 305×34 entry, 340×34 in all, at ((W−340)/2, 0.382·(H−34)); bullets 10 px apart from half a
bullet in, at most 30, and past 30 the right half of one more at the entry's left edge; the prompt
centred below; then the keyboard indicators, centred on the layout's (icon and name) width with
caps lock beside it, shown only while it is on. Fedora drops the prompt's trailing ':' and puts
5 px between entry and prompt. While the dialog is up, the spinner, the progress bar, the titles
and the percentage are not drawn (two-step's `display_password` stops the animation); messages
are, where they were.

Keyboard input (`synoik-splash/src/keyboard.rs`, shared by both drawers) comes from every evdev
node that reports keys, opened while a prompt is up and closed when none is, through one xkbcommon
state, as plymouthd's evdev path does: Return answers, BackSpace deletes a character, Ctrl+U and
Ctrl+W clear the entry, so do Ctrl+C and Ctrl+D (§3.2), Escape does nothing (plymouthd
shows its details), repeats type again, and the keyboards' LEDs follow the state. The initramfs
splash opens the keyboards when it starts and keeps them, reading them unarmed until a prompt:
once the real root loads its SELinux policy, its `kernel_t` may not open them (§7.1). Nothing is
grabbed: at boot no session has the keyboard yet. The foreground VT's keyboard is set to `K_OFF`
(by the initramfs splash; the outer lacks the privilege, §6)
meanwhile, its input flushed and its mode restored after (plymouthd's
`ply_terminal_set_disabled_input`), so the password reaches no line discipline: neither its input
queue nor, through echo, the VT's screen buffer. A VT already in `K_OFF` is left alone.

The layout is localed's: `XKBLAYOUT`, `XKBMODEL`, `XKBVARIANT` and `XKBOPTIONS` from
`/etc/vconsole.conf`, else the `Xkb*` options of `/etc/X11/xorg.conf.d/00-keyboard.conf` (Fedora
writes only `KEYMAP` to the former), else xkbcommon's defaults. plymouthd reads the first only and
falls back to the terminal without it. The initramfs carries the keymap compiled when the image
was built (§7.4). Compose sequences (dead keys) are not supported (§6).

### 4.3 Update modes

`updates`, `system-upgrade`, `firmware-upgrade` and `system-reset`: no spinner; a 400×5 progress
bar (white on #606060) in the spinner's place, at (0.5·(W−400), 0.7·(H−5)); a 30 pt title and a
12 pt subtitle ("Installing Updates…" and its siblings, then "Do not turn off your computer"),
one subtitle line apart; "N% complete" once progress is reported; `M` messages are suppressed. The strings are plymouth's; translating them through its gettext domain
is not built (§6).

Two divergences from bgrt, so that an update reads as the same screen as the boot it interrupts
rather than a different one cut in:

- **The firmware logo stays**, and the title pair sits above it, its subtitle's bottom 40 px over
  the logo's top (`TITLE_GAP`). Without a logo the pair is bgrt's: centred at 38.2 %. bgrt drops
  the logo and puts the pair where it was.
- **"N% complete" sits on the message line**, 10 px under where the spinner's bottom is, rather
  than 10 px under the bar: it takes the place of the boot's messages.
- **A mode change into an update mode or out of one moves as a login does**, with the outer's
  switch style (`SYNOIK_SYSTEM_TRANSITION`, `system-compositor.md`): the update screen is the
  front picture, as the greeter is. Going into an update it arrives over the boot screen — the
  curtain slides down over it, the depth lands it from near — and leaving, it goes the way the
  greeter goes at a login, off the screen it changes to. Both screens are drawn live (the spinner
  turning), over black rather than the plate. Leaving an update for a mode without a bar, the bar
  first fills from where the last `u` left it to 100 % over 150 ms (eased out, "N% complete"
  counting with it), as an update can end short of 100 % (measured 2026-10-20 on the harness:
  94 %). plymouth cuts in both cases.
- **The `C` waits for the screen.** The outer answers a mode change only once no display is
  still fading from it, at most 1 s later (`MODE_CHANGE_CAP`). The reboot after an update needs
  this: dnf5 ends with `u 100` and logind's `Reboot`, and the `C reboot` comes from
  `plymouth-reboot.service` (`Before=systemd-reboot.service`), so the machine reboots ~200-400 ms
  after it (measured on the harness), sooner than a fill and a fade; PackageKit sends its own `C`
  before it reboots, and waits the same way. A logind delay inhibitor cannot do it: logind starts
  the reboot, and with it the unit sending the `C`, only once the inhibitor is released.
- The outer's `PrepareForShutdown` fade is skipped on a display already showing the splash, so
  the update's reboot lets the change play out. The initramfs splash never sees an update mode:
  `system-update.target` waits for the outer (§2.3), and it answers every `C` at once.

### 4.4 Text and scale

16 px regular and 40 px light for titles, bgrt's sizes and weights, but in the shell's sans
(Adwaita Sans, the schema default of `org.gnome.desktop.interface font-name`) rather than bgrt's
Cantarell: the splash fades into the greeter and session, which set their text in it. A deliberate
divergence; the outer reads no user's settings, so it is the default, not a user's choice. The
layout's name beside the keyboard icon follows: plymouth blits it from `keymap-render.png`, a strip
of every layout name prerendered in Cantarell Bold 30 px, light grey, located by a table compiled
into plymouth; the splash sets the name xkbcommon gives (`English (US)`) in Adwaita Sans Bold 30 px
in the same grey, where plymouth's slice of the strip would be. Fedora's HiDPI rule is mutter's;
both jabuticaba displays come out at scale 1.
Images scale bilinearly at scale 2; text renders at device resolution.

## 5. Build order

The two kernel/systemd claims the design rests on were spiked first, with unit files only, on the
harness VM: the handover (§2.1) and surviving shutdown (§2.3). Both hold.

1. **Protocol server** in the outer: socket, framing, credential check, command table, per-connection
   ordering, idle-connection close at boot end, pid file. Tested headless against plymouth's own
   client binary and byte-level fixtures (every row of §3.2, the malformed inputs of §3.1).
2. **The splash model and drawing:** modes, messages, progress, the spinner clock, and the layout of
   §4 as conformance tests (positions per display size), then drawing through the outer's
   compositor with the theme's PNGs and the shell's text painter.
3. **Boot handover** (§2.1, and the rows of §2.2 that concern the boot: the deactivate/quit order,
   the two plymouth drop-ins, `/run/plymouth`) and the crossfade from splash to greeter or session;
   validated on the harness VM, which runs the same plymouth. §2.2's early start and shutdown
   settings come with step 4, the password-agent ordering with step 5.
   What the splash code leaves to it: `H` sets `Splash::frozen`, which the layout ignores — the
   outer stops redrawing; `$` after `H` restarts the spinner from frame 0 (`ply_throbber_start`
   resets its clock).
4. **Shutdown and offline updates** (§2.3): a `pkcon offline-update` and a dnf5 offline upgrade on the
   harness.
5. **Password prompts** (§4.2): the dialog's layout as conformance tests, the keyboard against a
   synthetic xkb state, both drawers' pixels; then on the harness, a LUKS volume unlocked in the
   initramfs (`x-initrd.attach`), one unlocked in the real root before the handover, and a
   `systemd-ask-password` after it, from the outer. Measured on the harness (2026-10-15, enforcing,
   a data disk with two LUKS partitions, keys injected on the virtio keyboard): the initramfs
   splash asked for both volumes, the second from the real root's `systemd-cryptsetup@` while the
   handover waited on `cryptsetup.target`; a `systemd-ask-password` started just before the
   handover had its dialog killed by it and asked again of the outer (§7.2), which took the
   answer; no SELinux denial.
6. **Emergency and rescue** (§2.4): `rd.break`, `systemctl emergency` and `systemctl rescue` on the
   harness — the splash ends, the shell answers on the serial console, and leaving it resumes the
   boot. A LUKS passphrase failed three times (`tries=3`, a volume the initramfs mounts) ends the
   same way: dracut's emergency shell on the serial console, the splash quit with the watermark
   kept, no dialog left on screen.

## 6. Open

- Scale: the splash is drawn at scale 1 everywhere. §4.4's rule (mutter's, as Fedora's plymouth
  applies it) is not implemented; both jabuticaba displays come out at 1 under it, a HiDPI display
  would not.

- Translation: plymouthd translates the update titles and "N% complete" through its gettext
  domain (`/usr/share/locale/*/LC_MESSAGES/plymouth.mo`) in the system locale. The outer draws
  them in English; nothing in the shell reads `.mo` catalogs yet.

- **SELinux denies plymouth's clients when the outer binds the name itself.** A socket the outer
  binds carries its domain, `unconfined_service_t`, and the targeted policy lets those clients
  (`init_t`: pid 1's unit-status updates and the units' `plymouth` calls) `connectto` only
  `plymouthd_t`: measured on the enforcing harness, pid 1's first update is denied, and an update
  screen gets no mode, progress or message. The listener the initramfs splash binds (§7) is
  `kernel_t`'s, and the outer adopts it: over a whole enforcing boot on the harness, no `connectto`
  was denied, and a unit's `plymouth display-message` got through. So the gap remains only where
  the initramfs still has plymouth (an image built before the install); the way out there is to label the
  listening socket `plymouthd_t` (`setsockcreatecon` before binding) or a policy module. An update
  screen under enforcing is untested on either path.
- **The outer cannot turn the VT keyboard off.** It runs unprivileged (`system-compositor.md`
  §1), and `/dev/tty0` is root's, `KDSKBMODE` `CAP_SYS_TTY_CONFIG`'s; so a password typed into the
  outer's dialog (a prompt after the handover, or at shutdown) also reaches the foreground VT's
  input queue. The initramfs splash runs as root and is not affected. The fix chosen is a small
  root helper that switches the keyboard off for the outer, keeping the outer unprivileged; it is
  not built.
- Compose sequences: neither drawer feeds keys through an xkb compose table, so a password typed
  with dead keys gets the dead keysyms' text, or none. plymouthd composes with the locale's table
  (`/usr/share/X11/locale`); the image would have to carry it.
- **A display plugged in after the first is lit stays dark until the outer.** The initramfs splash
  listens for DRM hotplug only while no display is lit (§7.3); the outer follows udev for all of
  them.
- Anyone can bind the abstract name while it is free (§2.1, between quit and bind). Clients do not
  check the server, so a squatter would receive passwords. The window is a few milliseconds and no
  prompt is possible inside it (§2.1), but it is not zero.

## 7. Phase 2: the initramfs

plymouth leaves the image; `synoik-splash`, a small program built from the outer's own splash code,
takes its place. It is plymouthd for the initramfs: it answers the protocol (§3), draws the splash
from the first DRM frame of the boot, and lives on across switch_root until the outer replaces it.

### 7.1 Across switch_root

The initramfs runs `synoik-splash.service`, a unit of its own: the real root's
`synoik-system.service` starts beside it, while it still draws (§7.2). Measured on the harness VM
(2026-10-14, systemd 259), with a probe service started from the initramfs:

- A unit started in the initramfs is the same unit after switch_root: it stays `active` with the
  same main PID, and its configuration is now the real root's file. The installer puts the
  splash's unit file in the real root too, with no `[Install]`, so the running unit has one.
- **systemd TERMs every initramfs process at the switch**, sparing only those whose `argv[0]`
  starts with `@` (plymouthd's mark). The unit runs `@/usr/libexec/synoik-splash @synoik-splash`.
- A process that survives has its root in the initramfs, which systemd empties. The splash
  needs no file after it starts: theme images are loaded up front, and its binary stays mapped.
- It runs as `kernel_t` (no policy is loaded yet), and keeps that label. Once the real root loads
  its policy, `kernel_t` may no longer `map` the card (`device_t`, devtmpfs's label before
  relabelling): the splash maps its buffers once, at setup, and a mapping it holds stays good.

### 7.2 The handover overlaps the outer's start

The splash turns the spinner until the outer is ready to draw, so the screen never waits on the
outer's process start or its GPU setup. The splash listens on an abstract socket,
`synoik-splash-handover`, and:

1. The real root's `synoik-system.service` runs `synoik-splash --hand-over` as root before the
   outer (`ExecStartPre=+`, `NotifyAccess=exec`). The splash answers it with the card (the same
   open file: master is shared from here) and one end of a socket pair, its **channel**; the
   helper puts both in the unit's fd store (`drm`, `splash-channel`). Both ends check that the
   other is root: anyone can bind an abstract name. Where no splash waits, the helper does
   nothing; a splash that is there but fails to answer fails the start, and the unit restarts: an
   outer that opened the card itself would not be master, a dark boot.
2. The outer starts from the real root's unit file, with all of its settings (user, groups,
   environment, directories, SELinux domain), and sets up Vulkan on the card's render node while
   the splash flips on. Until step 3 it reads no event from the card, where the splash's flips
   complete, and touches none of its framebuffers.
3. Ready to draw, the outer writes a stop over the channel. The splash makes no new flip, lets the
   one in flight land, and sends over the channel: plymouth's bound listener (`plymouth`), and a
   memfd with the splash state (`splash`: mode, messages, progress, frozen and shown flags, and
   the spinner's start on the monotonic clock). It removes its pid file first, closes its
   keyboards after (each close waits on the kernel's RCU), and exits.
4. The outer closes the splash's framebuffers (`CLOSEFB`, as after a crash, `system-compositor.md`
   §7), builds its DRM device, and draws the splash from that state at once, the spinner's phase
   continuing, without waiting for a session's first frame. It keeps the listener and the state
   in its fd store, so a crash restart keeps plymouth's name and resumes the splash, and removes
   the channel. Clients that connect meanwhile wait in the listener's backlog, not refused, and
   the socket keeps the label it got in the initramfs (§6, SELinux).

Measured on the harness VM (2026-10-16, 1280×800, frames captured on the host): the stop is
answered in 0.6 ms, and the outer's first frame follows the splash's last 79 ms later, one turn
of the spinner (≈67 ms) plus the outer's DRM setup and first draw; the next comes on the shared
spinner clock. Restarting the splash's unit into the outer instead held the last frame 314 ms:
systemd's stop and start, the outer's process start and its Vulkan setup, all with the screen
still.

**A password prompt up at the handover is asked again.** Its client's connection dies with the
splash, and systemd's plymouth agent (v259, `tty-ask-password-agent.c`) takes the EOF as `-EIO`,
logs it, and waits for the next change under `/run/systemd/ask-password`: alone, the asking unit
would wait until its own timeout. So once the outer answers, its unit try-restarts
`systemd-ask-password-plymouth.service` (an `ExecStartPost=`), and the new agent asks every pending
request of the outer; a crash restart is covered the same way. A drop-in the installer adds where
the initramfs has the splash orders the outer `After=cryptsetup.target`, so the crypttab volumes
are asked through the splash before it and their dialogs do not flash away and back. A request
made once logins are allowed goes to the wall agent, which stops the plymouth agent for good —
systemd's arrangement, as with plymouthd.

The real root's unit keeps its plymouth calls for a boot whose initramfs still has plymouth
(an image built before the install, or `--use-gdm` half undone). They are guarded on the pid file
naming a `plymouthd`, so a restart never sends `deactivate` or `quit` to a socket nobody is
answering yet, or to the outer itself.

### 7.3 Drawing without a GPU

The splash draws into dumb buffers on the CPU: no Mesa, no Vulkan driver, no shader compiler in
the image. Each connected display gets its preferred mode and two dumb buffers, flipped by page
flip. The layout is §4's, the outer's own code. The pixels are not identical to the outer's
GPU drawing: the restart's first frame can differ by a filter rounding where the images blend.

It probes the connectors itself (a `GETCONNECTOR` as master), so it does not depend on the kernel's
fbdev client (§2.2.2). It waits for the card on udev, as plymouthd does.

Text is drawn on the CPU too (`synoik-splash/src/text.rs`): the outer's shaping (cosmic-text,
the same metrics, hinting and weight axis) and its swash rasterization, from Adwaita Sans by path,
since the image has no fontconfig.

### 7.4 The image

A dracut module, `90synoik`, installs the program, its unit and the theme's images; `/etc/dracut.conf.d/synoik.conf` adds it and omits `plymouth` (≈3 MB smaller,
`initrd-size-and-time` measurements). The plymouth package stays installed, so `--use-gdm` brings
everything back with a `dracut -f`.

For the prompt, the image also carries the shell's sans (`AdwaitaSans-Regular.ttf`, 880 KB, the
file fontconfig picks for Adwaita Sans, at `/usr/share/synoik-splash/sans.ttf`), libxkbcommon, the
keymap compiled at build time (`synoik-splash --compile-keymap`, the system layout of §4.2, at
`/usr/share/synoik-splash/keymap.xkb`), and systemd's plymouth password agent, which dracut
installs only alongside its plymouth module. Like that module, it carries plymouth's client:
dracut's emergency shell quits the splash through it (§2.4). The splash's unit stands in for
`plymouth-start.service` there: it pulls the agent's path unit in, and is ordered before it and
before the console agent's, both of which test `/run/plymouth/pid` once, when they start. The
splash writes the pid file before it reports ready. A change of layout takes a `dracut -f`, as
the i18n module's copy of `vconsole.conf` already does.

### 7.5 Build order

7. **Crate split**: the protocol server and the layout move into `synoik-splash` (lib), with no
   smithay; the outer uses them from there.
8. **Handover** in the outer: the `plymouth` listener and `splash` state adopted at start.
9. **The program**: dumb buffers, the CPU painter, the handover; headless tests of the painter
   against the layout.
10. **Units, dracut module, installer**, and the boot on the harness: one PID from the initramfs
    to the handover, no empty plane at either end, the spinner's phase across it.

### 7.6 initramfs-tools (Debian)

Debian's initramfs runs no systemd: boot scripts, then `run-init` into the real root. The splash
is the same program, started and taken over differently.

- **Start.** The plymouth package's `init-premount/plymouth` starts plymouthd; the installer puts
  a script of the same name in `/etc/initramfs-tools/scripts/init-premount/`, which mkinitramfs
  copies over the package's, so the splash starts in its place, after udev, on the same switches
  (`nosplash`, `plymouth.enable=0`). It runs in the background, logging to `/dev/kmsg`, and the
  script waits for the pid file, written once the splash answers: plymouthd's readiness is its
  daemonizing. The package's other scripts stay: `panic` quits the splash, `init-bottom` sends a
  newroot it acknowledges. cryptsetup's askpass finds the splash with `plymouth --ping` and asks
  through `plymouth ask-for-password`.
- **The image.** A hook, `/usr/share/initramfs-tools/hooks/synoik`, installs what `90synoik` does
  (§7.4): the program, plymouth's client, the theme's images, the face, the keymap, and the GPU
  drivers by the plymouth hook's rules, which that hook adds only for a graphical theme. Debian's
  spinner has no `watermark.png`; both drawers then use Debian's logo, the file the plymouth
  hook puts in its place (`layout::WATERMARK_FALLBACK`).
- **The handover** is §7.2's: the splash is no unit's process, but it listens on the same
  abstract socket either way. Its pid file it cannot remove: past the switch its root is still the
  initramfs's, whose `/run` the switch moved into the real root, so the outer replaces the file by
  rename (tmpfiles has made the directory the outer's). Measured on Debian's systemd 261: a
  descriptor an `ExecStartPre=` puts in the store reaches `ExecStart=` under its name.
- **Ordering**: the same `After=cryptsetup.target` drop-in as on Fedora (§7.2). The
  real root's plymouth units are conditioned off as on Fedora (§2.2).
