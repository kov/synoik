# Boot splash: the system compositor takes over from plymouth

> Status: **design, not built.** Phase 1 of `system-compositor.md` §6: the outer takes the screen
> from plymouth after switch_root and owns it until power-off. Phase 2 (the outer in the
> initramfs) is out of scope here.

References: plymouth **24.004.60** (`~/Projects/plymouth`, Fedora 44's version; Fedora's patches
touch neither the protocol nor the layout math, only assets and HiDPI thresholds), systemd v259,
PackageKit 1.4.0 `client/pk-offline-update.c`, dnf5 5.4.6.0 `dnf5/commands/offline/offline.cpp`,
util-linux 2.41.5 `lib/plymouth-ctrl.c`, gdm 51.0 `daemon/gdm-manager.c`. Paths below are in the
plymouth checkout unless prefixed.

## 1. Decisions

- **Real root first.** plymouth keeps the initramfs; the outer takes over after switch_root, with
  no visible change, and keeps the screen through shutdown.
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

The client side is root-only by plymouth's design (`SO_PEERCRED` uid 0 or NAK,
`ply-boot-server.c:265-285`), so steps 1 and 3 run as `+`; the outer itself stays unprivileged and
applies the same check to its clients.

Prompts are dead between step 1 (plymouthd releases the keyboards) and step 5; a prompt pending
in plymouthd when it quits is lost (the agent sees EOF). The outer starts before
`cryptsetup-pre.target`, so no real-root volume asks inside that window.

A restarted outer (`Restart=always`) re-runs step 4 against its own socket: `Q` with no boot
splash showing is an ACK and nothing else, as is a `deactivate` with no plymouthd to answer it
(`-` on both).

### 2.2 Unit changes

| Unit | Change | Why |
|---|---|---|
| `synoik-system.service` | `DefaultDependencies=no`, `SurviveFinalKillSignal=yes` (§2.3); started as early after switch_root as its files allow (§2.2.1); `Before=cryptsetup-pre.target`; `Wants=systemd-ask-password-plymouth.path`; `WantedBy=` both `graphical.target` and `system-update.target`, with `Before=system-update-pre.target` | §2.1; prompts; offline updates; §2.3 |
| `plymouth-quit-wait.service` | masked | `plymouth --wait` sends nothing and returns only when the daemon closes it, with `TimeoutSec=0` before `multi-user.target`. The outer closes idle connections when the boot splash ends, but quit-wait may connect after that. |
| `plymouth-start.service` (real root) | `ExecStart=` neutralised by drop-in | If the initramfs had no plymouthd, a fresh one would race the outer for the card. |
| `plymouth-{reboot,poweroff,halt,kexec}.service` | `ExecStart=` replaced by `plymouth change-mode --reboot\|--shutdown` (a oneshot) | plymouth passes the shutdown mode only as a daemon argument (`--mode=`), never over the protocol. |
| `plymouth-quit.service` | stays `Conflicts=` | as now |
| `plymouth-switch-root-initramfs.service` | unchanged | its `R` is ACKed and ignored |

The installer writes these; `--use-gdm` removes them.

#### 2.2.1 What replaces the default dependencies

`DefaultDependencies=no` drops `sysinit.target`/`basic.target` and the shutdown conflict, so the
unit names what it needs instead: `After=systemd-journald.socket systemd-tmpfiles-setup.service
systemd-sysusers.service systemd-udev-trigger.service`, and `RequiresMountsFor=` its binary,
Mesa's ICD and libraries, and its cache dir (`/usr`, `/var/cache`). The card has no systemd device
unit on Fedora (udev does not tag DRM cards for systemd), so the outer waits for it itself, from
its udev monitor, when it is not there yet; at switch_root it normally is (virtio-gpu loads in the
initramfs at ≈1.1 s on jabuticaba).

`Before=cryptsetup-pre.target` is a cycle wherever one of those paths lives on an encrypted
volume (a separate crypt `/var`). The installer orders the outer before `cryptsetup-pre.target`
only when they do not; otherwise the outer starts after the volumes, and their prompts are
answered by the initramfs plymouthd, still on screen.

### 2.3 Shutdown and offline updates

- **The outer keeps the screen until power-off.** Today it is stopped for shutdown while systemd's
  fd store keeps master, so the shutdown plymouthd cannot draw either: nothing shows the shutdown
  splash. With `DefaultDependencies=no` the outer is not stopped by `shutdown.target`;
  `SurviveFinalKillSignal=yes` keeps it through systemd-shutdown's final kill (plymouth's `@`
  argv mark protects only root processes, `systemd src/shared/killall.c:91-100`).
- On `PrepareForShutdown` the outer crossfades from the session to the shutdown splash
  (it fades to black today) and spins until power-off. dracut's late `H` freezes the last frame.
  The mode (`C reboot|shutdown` from the rewritten units) arrives only after the display manager
  stops, long after `PrepareForShutdown`; the fade does not wait for it, because bgrt's boot,
  shutdown and reboot screens are identical (§4.1). Only an update's end changes the screen.
- **Offline updates** (`system-update.target`) start the outer too, and PackageKit and dnf5 drive
  its update screen through `C`, `u` and `M` (§3.3).
- Open: the outer keeps files on the root filesystem mapped, so dracut's shutdown can only remount
  it read-only. A binary replaced by the update and still mapped may block even that. To verify on
  the harness.

### 2.4 Handing the console back

`plymouth quit` *without* `--retain-splash` comes from emergency.service, rescue.service,
dracut-emergency and sulogin: "give the text console back". The outer drops master, removes its
copy from the fd store (`FDSTOREREMOVE=1`) and closes every DRM fd, so the kernel's last-close
restores fbcon, and stays down until restarted. tty1 is left in `KD_GRAPHICS` by plymouthd's
retained quit; switching it back to text needs root, so the outer's binary does it as
`ExecStopPost=+synoik system-compositor --release-console` (`KDSETMODE KD_TEXT` on tty1).

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
| `C` | change mode: `boot-up`, `shutdown`, `reboot`, `updates`, `system-upgrade`, `firmware-upgrade`, `system-reset` | switch screen (§4.3); unknown modes ACKed and ignored |
| `u` | update progress 0-100 | progress bar |
| `M` / `m` | show / hide message (exact match) | message list; an empty `M` clears it (systemd's FIDO2 code relies on that) |
| `*` | ask for password, prompt optional | the password dialog (§4.2); ANSWER on Enter, NO_ANSWER when the splash cannot prompt so the client asks elsewhere |
| `c` | cached passwords | NO_ANSWER — **never NAK**: systemd's agent turns anything but ANSWER/NO_ANSWER into -EIO |
| `Q` | quit, 1-byte retain flag | ends the boot splash (ACK when done); without retain, §2.4 |
| `U`, `A`/`a`, `R`, `S`, `!`, `D`, `r`, `l` | status, pause/unpause progress, newroot, sysinit, error, deactivate, reactivate, reload | ACK, no effect (`R` must never chroot) |
| `W`, `K` | question, keystroke watch | NO_ANSWER |
| `L` | ignore keystroke | ACK |
| `V` | has active VT | NAK |
| other | | NAK |

A cancelled password (Ctrl+C / Ctrl+D) answers NO_ANSWER, not plymouthd's `"\x03"`, which
systemd's agent would take as the password. Answers are not cached.

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

- **Background:** black. With an ACPI BGRT table (absent in Limina VMs), the firmware logo,
  centred horizontally with its centre at 38.2 % of the height.
- **Spinner:** the theme's 30 `throbber-*.png` frames (the installed set; Fedora replaced the
  checkout's), 32×32, one turn per 2 s, top-left at (0.5W−16, 0.7H−16).
- **Watermark:** `watermark.png` (149×43, the same file as `/usr/share/pixmaps/fedora-gdm-logo.png`),
  top-left at (0.5W−74.5, 0.96H−41.28), in every state.
- **Messages:** centred, 10 px under the spinner.
- **No fades.** On quit the spinner simply stops: plymouth's retained frame is background +
  watermark, which is the frame the outer crossfades from at login and to at shutdown.

### 4.2 Password dialog

A 35×34 lock beside a 305×34 entry, 340×34 in all, at ((W−340)/2, 0.382·(H−34)); bullets 10 px
apart, at most 30; the prompt below, then the keyboard-layout and caps-lock indicators. Fedora
drops the prompt's trailing ':' and puts 5 px between entry and prompt.

Keyboard input comes from the outer's own evdev fds (as the chord matcher's, §2.1 of
`system-compositor.md`) through xkbcommon, with the system layout from localed
(`/etc/vconsole.conf` / `/etc/X11/xorg.conf.d/00-keyboard.conf`), as plymouth does. The outer
never grabs; at boot no session has the keyboard yet.

### 4.3 Update modes

`updates`, `system-upgrade`, `firmware-upgrade` and `system-reset`: black background (no firmware
logo), no spinner; a 400×5 progress bar (white on #606060); a 30 pt title and a 12 pt subtitle
("Installing Updates…" and its siblings, then "Do not turn off your computer"); "N% complete". The
strings are plymouth's, translated through its gettext domain.

### 4.4 Text and scale

Cantarell Regular 16 px and Cantarell Light 40 px for titles: what plymouth's pango label plugin
renders from the real root (the initramfs's freetype plugin uses Noto Sans, but nothing in phase 1
draws there). Fedora's HiDPI rule is mutter's; both jabuticaba displays come out at scale 1.
Images scale bilinearly at scale 2; text renders at device resolution.

## 5. Build order

0. **Spike with unit files only, on the harness VM**, before any code: the two kernel/systemd
   claims everything above rests on, both untested. (a) `ExecStartPre=+-plymouth deactivate` and
   `ExecStartPost=+-plymouth quit --retain-splash` around today's outer: plymouthd's frame stays on
   screen until the outer's first modeset, no black frame or fbcon flash (screen capture from the
   host). (b) `DefaultDependencies=no` + `SurviveFinalKillSignal=yes`: the outer is still drawing
   when `systemctl poweroff` reaches the end (its fade stays up; the journal shows no SIGKILL).
   If either fails, this design changes first.
1. **Protocol server** in the outer: socket, framing, credential check, command table, per-connection
   ordering, idle-connection close at boot end, pid file. Tested headless against plymouth's own
   client binary and byte-level fixtures (every row of §3.2, the malformed inputs of §3.1).
2. **The splash model and drawing:** modes, messages, progress, the spinner clock, and the layout of
   §4 as conformance tests (positions per display size), then drawing through the outer's
   compositor with the theme's PNGs and the shell's text painter.
3. **Boot handover** (§2.1, §2.2) and the crossfade from splash to greeter or session; validated on
   the harness VM, which runs the same plymouth.
4. **Shutdown and offline updates** (§2.3): a `pkcon offline-update` and a dnf5 offline upgrade on the
   harness.
5. **Password prompts** (§4.2): a `systemd-ask-password` at boot on the harness, and a LUKS data
   volume there.
6. **Console handback** (§2.4): `systemctl emergency` on the harness.

## 6. Open

- SELinux: on an enforcing system, confined clients may be allowed `connectto` only on
  `plymouthd_t`. jabuticaba is permissive; check the policy before enabling enforcing.
- Anyone can bind the abstract name while it is free (§2.1, between quit and bind). Clients do not
  check the server, so a squatter would receive passwords. The window is a few milliseconds and no
  prompt is possible inside it (§2.1), but it is not zero.
