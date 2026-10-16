# The system compositor

> Status: **design agreed 2026-08-15; built far enough to replace GDM** — sessions nested and
> scanned out unredirected on every connected display, each session deciding its own display
> layout, the screen following logind's active session with a crossfade, surviving a crash of
> either side, and switchable by the outer's own chord where there are no VTs (§10). The greeter
> is `greeter.md`. The outer is an unprivileged system user outside logind (§9 S1);
> `scripts/install-display-manager.sh` makes it a machine's display manager.

A second synoik configuration — the **system compositor** — owns the display for the machine's
whole lifetime, from bootloader hand-off to power-off. It hosts the greeter, heralds system
events (boot, shutdown, offline updates), and **nests the per-user session compositors as
clients**, so login, user switch, lock and logout are animated transitions instead of the blink
we inherit from the GDM model.

Target platform for the first cut is **Limina**, where we control both sides.

---

## 0. The premise everything rests on: cooperative nesting

Ubuntu's Mir shipped this exact architecture (~2013) and abandoned it. The tax that killed it was
nesting *adversarially* — an outer compositor that must assume the inner knows nothing, so every
capability has to be laundered through a general-purpose protocol with general-purpose semantics.
The pass-through surface never stopped growing.

**We do not have that problem.** The inner compositor is synoik, built from the same tree,
versioned in lockstep. It can be told it is nested and adjust. That means:

- the outer↔inner interface is a **private IPC that ships our own structs**, not a public
  Wayland protocol we have to generalise and then live with;
- capabilities can be **handed over wholesale** rather than proxied — see §2, where the inner
  gets real evdev fds rather than forwarded events;
- the inner gets a third backend alongside `tty` and `headless`: **`nested`** (`backend::nested`).

ChromeOS is the existence proof that the shape works (single compositor, no VTs, `frecon` in the
boot-splash/recovery-console role). Mir is the proof that it only works cooperatively.

**Corollary, and it is load-bearing: scope is synoik-only.** The day we want to nest a foreign
compositor, most of this document stops applying. That is an acceptable trade and should be
re-examined explicitly rather than drifted into.

---

## 1. Responsibility split

The outer is a **display arbiter and herald**, not a second full shell. In steady state it holds
DRM master and the CRTC, composites nothing, and runs a chord matcher. It grows a UI only when it
is fronted (greeter, transitions, system heralds).

Keeping it thin is not an aesthetic preference — it is what makes restart fast and reconnect
(§7) cheap.

**No privileged process.** The outer runs as an unprivileged system user (as GDM does) in the
`video` and `input` groups, outside logind's session model (§9 S1). It needs root for nothing:

- **DRM master** — the first opener of a card with no master becomes master; no capability check.
  The outer opens it before any session exists and never drops it. A later opener, including the
  active user (who holds a `uaccess` ACL on the card), can open it but cannot take master.
- **Crash gap** — master dies with the last reference to the open file, and a masterless card can
  be claimed by anyone with access. So the outer stores its DRM fd in systemd's fd store
  (`FileDescriptorStoreMax=`, `FDSTORE=1`); systemd's reference keeps the file and its master alive
  across a crash, and the restarted outer gets it back. The inherited fd cannot `SET/DROP_MASTER`
  unprivileged (the kernel checks the opener's pid), which costs nothing: the outer never drops it.
  The store must outlive a crash *loop* too: a unit that hits its start limit fails, and a failed
  unit's store is flushed — master lost — so the unit sets `StartLimitIntervalSec=0` and
  `FileDescriptorStorePreserve=yes`. Releasing master is then a deliberate
  `systemctl clean --what=fdstore`.
- **Framebuffer handover** — framebuffers belong to the open file, so the restarted outer inherits
  its predecessor's, including the session frame on screen. It `CLOSEFB`s them (Linux 6.8): the
  handle goes, the scanout stays until the next flip. `RMFB` would turn the plane off.
- **Session switch** — a polkit rule grants the outer's user `org.freedesktop.login1.chvt`, the
  action logind checks for `ActivateSession`.
- **Its own input** — the `input` group, for the chord matcher (§2.1).

The inner's devices come from logind (§2), so there is no device-brokering helper either.

**No fbdev console may be bound to the outer's card.** A VT returning to text mode (`KD_TEXT`)
makes the kernel force-restore fbdev over whoever holds master — `drm_fb_helper_set_par`'s
deliberate ABI exception for X servers that switch to text before dropping master. Measured: with
the session alive and active and the outer master, `KDSETMODE KD_TEXT` on its VT put fbcon's
framebuffer on the plane; with fbcon unbound the same ioctl left the plane alone. The `KD_TEXT`
comes from logind, and the fbdev restore runs inside that very ioctl. logind issues it when the
session's *controller* lets go — measured: a controller exiting put its VT to text while the
session lived on, and the session's end later issued nothing. The kernel only touches the screen
for the foreground VT, so it is the fronted inner crashing, restarting or calling
`ReleaseControl` that shows the console, logout or not. logind has no setting for this: in
systemd 259 `session_prepare_vt`/`session_restore_vt` are unconditional for any session with a VT.
Sessions cannot avoid owning a VT either: on a seat with VTs logind refuses a session with none
(`CreateSession` → `InvalidParameter: VTNr`, measured on seat0). So there is no fbcon on that card,
by one of the setups §9 S6 settles.

| | Outer (system compositor) | Inner (session compositor) |
|---|---|---|
| DRM master, CRTC, planes | **owns** (opens it itself, §1) | never touches |
| Modeset | executes | **decides** (§4) |
| Input devices | own opens, for the chord matcher | **takes real evdev fds from logind** |
| libinput config | none (chord matcher only) | **owns** |
| Greeter | **hosts** (as a client) | — |
| Session windows, shell UI | none | **owns** |
| logind session | **none** — system user, outside logind (§9 S1) | **is one** |
| Audio / USB / webcam ACLs | none | **owns** (follows logind `Active`) |
| Xwayland | none | **owns** |
| Steady-state scanout | passes through (unredirected) | **renders directly to scanout** |

Steady state is **unredirected**: the fronted session's buffer goes straight to the scanout plane,
one flip, no outer compositing, no added latency. The outer only composites during transitions and
while it is itself fronted.

The inner allocates its scanout buffers on its own Vulkan device and registers each with the outer
once, as a dmabuf; a frame is then a buffer id plus the render's `sync_file`, which the outer hands
to the atomic commit as `IN_FENCE_FD` — the kernel orders the flip after the render, and neither
side blocks on it. virtio-gpu lacks `DRM_CAP_ADDFB2_MODIFIERS`, so those buffers are LINEAR and
`AddFB2` names no modifier (`framebuffer_from_dmabuf`). A buffer on screen outlives the process
that rendered it. Probe for the bare path: `synoik-vk/examples/xproc_scanout_send.rs` +
`spikes/xproc_scanout_recv.c`.

---

## 2. Input

**Do not forward events.** A packet stream cannot feed libinput, which wants real evdev fds; the
inner would end up reimplementing device configuration, and GNOME's model puts per-device config
(accel profile, tap-to-click, tablet mapping) in the *session*.

**Each evdev node is opened twice, independently.** The outer opens its own (`input` group, §1);
the inner takes its own from logind (`TakeDevice`), exactly as synoik's tty backend does today.
Two independent `open()`s — not `dup()` — so each has its own event queue and neither steals from
the other. Both see every event.

The inner runs real libinput on real fds. The outer's steady-state input handling is a chord
matcher and nothing else.

### 2.1 The escape hatch must survive a hung inner

Cooperative "inner asks the outer to take over" IPC is the normal path and is simpler. But it
fails in exactly the case that needs it — inner wedged. The outer keeping its own fds gives a
hardware-path chord that works against a hung session.

**Both paths ship.** IPC for the normal case, chord for recovery.

### 2.2 Revocation is a security requirement, not a courtesy

If user A's inner compositor holds live evdev fds while user B is fronted, **A keylogs B.**

logind enforces it: when the outer fronts B it calls `ActivateSession(B)`, and logind
`EVIOCREVOKE`s every device A took (`PauseDevice` type `force`, no ack awaited — measured, §9 S1).
Enforced by logind, never by inner good behaviour — the boundary being crossed is a user boundary,
and that is the one place cooperation is not a sufficient argument. The corollary is that the
outer must never front a session without activating it.

### 2.3 Smaller obligations

- **Hotplug.** The inner watches udev and takes new devices from logind, as today.
- **Modifier state.** Handover needs an explicit modifier reset, or a session that was holding
  Ctrl when it lost focus leaves it stuck.

---

## 3. Output and DRM model export

The inner decides the display configuration; the outer executes it. The protocol cannot be "set
1920x1080" — GNOME's model needs the connector-level truth:

- EDID; connector name (**this is `monitors.xml`'s identity key**); mode list; physical size;
  VRR range; panel orientation; HDR metadata.

Plus an **atomic test/commit with revert-on-timeout**, because the "Keep this configuration?"
dialog is a real requirement.

**Arbitration policy: the fronted session's config wins, and the outer inherits it** rather than
imposing its own. The greeter, each session, and the outer's own transition rendering all want a
mode; one rule settles all of them. Until the fronted session has said what it wants (it is still
starting), the outer keeps the layout it has, so a login's fade never modesets under it.

### 3.1 Presentation and frame pacing

An unredirected fullscreen client schedules its frames off presentation feedback. **Frame
callbacks and `wp_presentation` timestamps must reflect the real flip** — the outer passes the
hardware timing through and adds no synthetic clock of its own. Getting this wrong mis-paces the
inner in exactly the steady state the whole design exists to optimize.

The same applies to the capability protocols the pacing depends on: **VRR and tearing-control
have to reach the inner end to end**, or the inner will believe it is on a fixed-refresh,
no-tearing output and schedule accordingly.

During transitions the outer is compositing, so it owns the timing and reports its own — but a
snapshotted source session (§5) is not rendering at all, and the target session must not be told
it is on screen before step 8.

---

## 4. Modeset policy

### 4.1 Default: never modeset for a session switch

Two sessions only need a modeset if they differ in **resolution or refresh**. Differences in
scale, rotation, or layout are compositor-side and free.

**So the CRTC sits at the panel's native mode permanently, and sessions differ only by scale and
logical size.** Session switching then never modesets, and §5 is a true crossfade.

The fallback path exists for the genuine resolution-change case (a forced-lower mode for a game,
an old panel). It is rare, it looks different, and that is honest.

### 4.2 When a modeset is unavoidable: hide it under black, not under content

A modeset on the same connector generally drops the link and re-trains it — DP especially, HDMI
re-syncs. The panel goes dark for ~100 ms to a couple of seconds **regardless of how carefully
the before-and-after frames match**. Matching content buys nothing.

So the transition's midpoint is deliberately **black**, and the modeset happens there. A blank
during black is invisible by construction. Fade-through-black between two sessions is a
legitimate transition in its own right, so this costs nothing aesthetically.

### 4.3 Mechanics

**Atomic mode + fb in one commit.** Pre-allocate and render the outer's plate into a framebuffer
sized for the *new* mode before committing, then set mode and fb together — otherwise there is a
moment with no valid fb for the new mode.

---

## 5. The transition state machine

For a switch from session A to session B. Steps 3–4 are **skipped entirely** under the §4.1
default, which is the common case.

1. **Snapshot A.** Capture A's final frame into an outer-owned texture. Animate *that*, not A.
   Three wins: no A frames arriving mid-animation at the wrong size; no dependency on A being
   responsive (**a hung session still animates out**); A can be suspended for the whole
   transition. **`ActivateSession(B)` here** (§2.2): logind revokes A's input and resumes B's in
   one step, so no keystroke typed during the animation reaches A.
2. **Animate out** — outer composites, current mode still set.
3. *(mode-change only)* **Converge to black.**
4. *(mode-change only)* **Modeset**, atomic mode+fb per §4.3. Panel may blank; invisible.
5. **Tell B its output config**, then hand B the **scanout dmabuf-feedback tranche** — before the
   in-animation ends, so B's first unredirected frame does not force a reallocation right at the
   handoff.
6. **Wait for B's first correctly-sized commit.** Gate the in-animation on that frame, **never on
   a timer.** Starting on a timer and hoping is how a wrong-sized frame lands at the most visible
   moment in the entire design.
7. **Animate in.**
8. **Drop to unredirected.** Composited → direct scanout is a plane reconfiguration, not a
   modeset; cheap, given step 5.

### 5.1 Output topology changes

A may have two monitors and B one. Then it is not a modeset, it is outputs appearing and
disappearing mid-transition.

**The outer keeps every output lit with its own plate** rather than DPMS-ing one off
mid-animation, and settles the topology only once the target session is fronted.

---

## 6. Boot: separable, and deferred

The highest-value slice — greeter, login, user switch, logout, lock — needs **none** of the
early-boot work, and carries all of the architectural risk (§2, §3, §9 S1). Build that first.

Plymouth replacement is built in two phases, both in `boot-splash.md`: the outer keeps the screen
from switch_root to power-off, and a splash of ours holds it in the initramfs. In full it means:

- **Living in the initramfs** (`boot-splash.md` §7): a small CPU-drawing splash program holds the
  card from the boot's first frame, survives `switch_root` as plymouthd does, and the real root
  restarts its unit into the outer, master and frame crossing in the fd store. No Mesa or Vulkan in
  the image.
- **Plymouth's other jobs come with it**: the FDE passphrase prompt (`systemd-ask-password`
  agent), fsck progress, `system-update.target` rendering.
- **The seamless part is not fbcon** — it is `simpledrm`/`efifb` → real driver. Flicker-free means
  carrying the same framebuffer across the driver switch without a CRTC reset.
- **Dropping VTs costs the emergency console.** `drm_panic` (infra ~6.10, QR ~6.12) is the modern
  answer, but it is **per-driver**. Our daily driver is a VM on virtio-gpu/venus — **verify that
  driver implements `drm_panic` before betting on it.** On a laptop with no serial, "kernel
  panicked and the screen is black" is a bad day.

---

## 7. Reliability: the outer is a single point of failure

An outer crash takes the display for *every* session, and Wayland has no reconnect story —
clients die with their compositor. Today a shell crash costs one session; here it costs the
machine.

**Cooperation gives us a fix that general nesting cannot have.** We do not need a general
reconnect protocol — we need one for exactly one known client. The inner keeps its own render
device and its own dmabufs; an outer restart needs re-master, re-import, and a re-attach
handshake.

That turns "outer crash = the machine's display is gone" into "outer crash = a flicker and a
reattach." Design the private protocol for reconnect **from the start**; retrofitting it means
the inner has already been written assuming its Wayland connection is its lifetime.

---

## 8. What this is bought for

**Cross-session animation, and only that.** Seamless DRM-master handoff between separate
compositors — no nesting at all — is achievable; the GDM blink is modeset and fb churn, not a law
of physics, and it is an order of magnitude less work.

Nesting is justified if and only if we want the herald to animate *over* a live session. We do.
But if the scope ever contracts to "just don't blink", this whole design should be dropped for
the handoff approach rather than trimmed.

### Cost that is *not* a reason to hesitate

N logged-in users already means N shells today. The delta here is **one** extra compositor, not
N. (An earlier draft cited host OOM from scanout churn as a constraint. That was a bug in the VMM
gfx stack, since fixed; it is not a live constraint and should not be raised as one.)

---

## 9. Open questions

**S1 — resolved: the outer runs outside logind, unprivileged.**
Measured 2026-10-10 on stock Fedora 44 (systemd 259.5, kernel 6.19) on a VT-less seat (`seat1`:
GPU and sound attached, `CanTTY=no`) — the same logind switching path a `CONFIG_VT=n` seat0 takes:

- `ActivateSession` switches between user sessions; `ActiveSession` tracks it, and the GPU and
  sound `uaccess` ACLs move to the active user every time. A newly created session goes `Active`
  on its own.
- A process holding the card **outside any session** does not interfere with switching or ACLs.
- A greeter-class session that is `Active` strips every user's ACLs and grants none, and logind
  force-revokes a session's devices whenever it is not `Active`. So the outer can be neither a
  permanently-`Active` session nor a logind device client.
- Sessions that `TakeDevice` get `PauseDevice` (type always **`force`**) for card and evdev on
  switch-away and `ResumeDevice` on switch-back; logind never waits for `PauseDeviceComplete`.
- An unprivileged `video`-group system user opening an unmastered card gets master and modesets.
  With master held, the active user's open gets no master and `drmSetMaster` fails `EACCES`.
- SIGKILL of the master holder frees master for the active user to claim; with the fd in systemd's
  fd store it does not, and the restarted holder gets the fd back still master.
- Real keyboard and pointer (virtio-input, kov typing live): only the active session's
  `TakeDevice` fds deliver events. On switch-away logind revokes them (`read()` → `ENODEV`) and
  `ResumeDevice` hands the incoming session fresh fds. A `video`+`input` system user reading the
  same nodes by plain `open()` saw every event throughout. Input nodes carry **no** `uaccess` ACL:
  evdev gating is `TakeDevice` + revoke alone, so the inner must take input through logind.
- Without a polkit rule the system user's `ActivateSession` is denied; logind checks
  `org.freedesktop.login1.chvt`, and allowing it is sufficient.

Fallback if VT-less operation causes trouble: keep a VT and run the outer and every session it
nests on that **single VT**, so the VT never switches and session switching stays the outer's.

Bring-up notes: GDM spawns a greeter on any new graphical seat, so it must not run on the outer's
seat. With no display manager running, `loginctl attach` writes the udev rule but the seat does not
appear until `udevadm trigger` re-adds the devices.

**S2 — resolved: the greeter is a nested session, and PAM is a root daemon's.** The greeter is
`synoik --greeter` in a `CLASS=greeter` logind session, attached to the outer like any session; a
small root login daemon (`synoik-login`, speaking GDM's D-Bus subset) runs PAM and opens user
sessions. The outer stays unprivileged and never sees a secret. Design: `greeter.md`.

**S3 — Suspend/resume and lid across the boundary.** Who owns the inhibitor, and does the shield
still ride the flip (cf. the "a suspend waits for a PRESENTED shield" finding) when the presenting
compositor is the outer and the locking one is the inner?

**S4 — Lock screen: inner or outer?** Putting it in the outer is the security win (unlock is not
inside the session, which is GDM's structural problem today). But we already implement a lock
screen in the inner, and moving it means the outer grows real UI — against §1.

**S5 — Multi-seat.** Not considered anywhere above.

**S6 — resolved: no fbcon on the outer's card, two supported setups.**

- **Stock kernel, VTs present** — sessions keep their VTs; the install puts `fbcon=map:9` on the
  kernel command line: every console maps to a framebuffer that does not exist, so fbcon binds to
  none, including through its deferred takeover (`fbcon.c` `do_fb_registered` only takes a
  framebuffer some console maps to). Measured on the harness VM (2026-10-14, VT kernel, fbdev on):
  without it, fbcon takes over at the first console output and `KD_TEXT` on the foreground VT puts
  fbcon's framebuffer on the plane, where it stays after `KD_GRAPHICS`; with it, only the dummy
  console binds and the plane keeps the outer's framebuffer. The kernel's fbdev client stays,
  and with it the first probe of the displays at boot, which plymouth needs to find them
  (`boot-splash.md` §6) — why this, not `drm_kms_helper.fbdev_emulation=0` (which removes the
  client) or a root unbind of `/sys/class/vtconsole` at boot (which a deferred takeover outlives).
  The cost: no kernel text console on the display — boot messages and an emergency shell go
  unseen there. `--use-gdm` removes the argument.
- **Enhanced-tier Limina guests** — `CONFIG_VT=n`: no VTs, so sessions are VT-less as on the S1
  seat and there is no fbcon (measured, §10: seat0 reports `CanTTY=no`, logind takes VT-less
  sessions there, and the outer's chord is the switch). If VT-less operation turns up too many
  problems, these guests fall back to the stock setup above.

---

## 10. What is built

Sessions nested and unredirected, switched by logind with a crossfade — the steady state of §1,
the fronting rule of §2.2, the no-modeset transition of §4.1/§5 and the reliability of §7.

- **Wire** (`system_compositor::protocol`): one JSON message per `SOCK_SEQPACKET` packet, fds as
  `SCM_RIGHTS` on the same packet, version refused on mismatch. `Hello`/`Welcome` (render node,
  connector, EDID, modes), `AddBuffer`/`RemoveBuffer`, `Present` (+ fence), and back `Presented`
  (kernel flip time and sequence, untouched, §3.1) or `Discarded`. Each `Present` is answered
  exactly once, and only by its own buffer id: a frame shown again is not re-answered.
  `CursorImage` (a memfd), `CursorPosition` and `Present`'s `cursor` carry the pointer (below).
- **Outer** (`synoik system-compositor`, `resources/synoik-system.service`): quits plymouth
  (`--retain-splash`, as GDM does), opens or recovers the card, lights every connected display
  with its plate, and flips the buffers of the session logind has made active on seat0, following
  `ACTIVE=` in `/run/systemd/seats/seat0` by inotify (`sd_login_monitor`'s mechanism). A
  restarted outer adopts each lit CRTC at the mode it runs, with no modeset. Any number of sessions attach; each names its logind
  session in `Hello`, accepted only if that session is on seat0, owned by the peer's uid, and —
  when the peer runs in a session scope — that scope's. A background session's newest present is
  held unanswered (so its slot stays reserved) and the one it supersedes discarded. The screen
  never shows a buffer the inner may be drawing into: only a held frame, or the last one answered
  `Presented`.
- **Displays** (§3): the outer describes every connected connector — name, EDID, physical size,
  modes — in `Welcome`, and again in `Connectors` whenever udev reports a change on its card
  (plug, unplug, a new mode list, which is all a resized VM window is). Each session answers with
  `Configure`: the connectors it lights, each at one of its modes, and whether they are on at all
  (an idle blank, `PowerSaveMode`). The outer applies the fronted session's; the session decides
  as the tty backend does (`backend::nested`: off, the lid, the live-applied config,
  `monitors.xml`'s saved mode, `pick_mode`), so GNOME Settings' resolution and "off" work.
  Each display flips, fades and shows its plate on its own; a switch to a session with another
  layout cuts a display whose mode, or on/off, changes through its plate (§4.2's black is not
  built), and crossfades the rest. A frame for a display not lit at its size waits unanswered, as
  a background frame does.
- **Switch** (`system_compositor::compose`, the outer's own Vulkan device on the render node): on
  a change of active session the outer snapshots what is on screen into its own texture (§5 step
  1; mid-transition, the frame on screen) and moves it out for the incoming session's frame, into
  two outer-owned scanout buffers, then flips the session's frame itself (§5 step 8). How it
  moves is `SYNOIK_SYSTEM_TRANSITION` on the outer's unit (`compose::Style`), and which way the
  logind classes of the two sessions decide (`compose::Way`), the greeter being the front picture:
  - `curtain` (the default): the greeter is a curtain over the sessions, as gnome-shell's lock
    screen is over one — a login or an unlock slides it up off the session, a logout or a switch
    user slides it down over it; 300 ms ease-out-quad (`CURTAIN_SLIDE_TIME`, `screenShield.js`).
    Between two users' sessions the plate is the curtain, down over the one and up off the other:
    two slides, each 300 ms eased on its own.
  - `depth`: the greeter flies past, growing to 3× as it fades out (by the square of the
    motion, so it is mostly gone early), as the session lands from a distance (0.75× and
    transparent, as gnome-shell's session start, `_startupAnimationSession`), and the
    same backwards for a logout or a switch user; between users the outgoing session leaves as the
    greeter would. 500 ms ease-out-quad (`STARTUP_ANIMATION_TIME`, `layout.js`).
  - `fade`: a crossfade, 250 ms ease-out.

  Every motion draws over the plate. The boot splash leaves as the greeter does at a login, and
  the shutdown splash arrives as it does at a logout (Shutdown, below); from the plate a switch
  crossfades whatever the style. `SYNOIK_SYSTEM_TRANSITION_SLOW` multiplies every duration, to
  watch one closely. The target is the incoming session's held frame, else its next present,
  gated on its render fence polled readable (§5 step 6), and it is live: each frame the session
  presents during the transition replaces it, answered `Presented` by the flip that shows it, so
  whatever the session animates meanwhile (an unlock, its startup) plays through. A fronted frame
  that replaces the plate fades in the same way. Without a usable GPU a switch is a cut.
  Switch User leaves the desktop on screen and the lock screen held: the session sends its
  shield's frame as a `Hold`, which the outer keeps unshown as the frame that session comes back
  to, and goes to the login screen on `Held`. So the curtain comes down over the desktop, and a
  return shows the lock screen (a divergence from `activateSwitchUser`, which waits for the shield
  to be painted, `systemActions.js:476-479`). Should the switch not happen, or `Held` not come,
  the held frame is presented for real.
  A session leaving for the login screen holds gsd-power's blank until it has been paused, or
  for 2 s if the switch fails (`Synoik::leaving_for_login`). gsd-power blanks the moment the
  shield is up, and Switch User raises the shield just before the switch. A blank that won that
  race would take the display off, and the switch would start from the plate as a crossfade.
  GNOME has the same race, hidden there by the VT switch.
  Measured on the harness (2026-10-16, `login.sh`, `switch.sh` both ways and `logout.sh` per
  style): each motion runs its full length at the display's 8 ms with no frame back; a login holds
  the greeter's verified prompt about 0.45 s until the session's first frame, then moves it away,
  with no black between them.
- **Cursor plane**: a connector whose CRTC has a cursor plane advertises its buffer size
  (`cursor_size`, the driver's `DRM_CAP_CURSOR_WIDTH`). The inner then leaves its pointer out of
  its frames and puts it on the plane instead. A new image (`CursorImage`, drawn on the CPU from
  the cursor's own buffer, xcursor memory or the client's shm, at its size on the display and
  turned with a rotated display, bilinear, as mutter's native cursor renderer scales and turns its
  sprites) is only kept by the outer; the position shows it. Each `Present` carries the cursor's
  position for that frame, so a frame and its cursor change in one commit and a moving pointer
  never holds a frame back; a pointer that moves with no frame to draw sends `CursorPosition`,
  which the outer commits alone, or after the commit in flight. A pointer the plane cannot take
  — a drag icon, a cursor surface with subsurfaces, a buffer with a transform of its own, one
  larger than the plane, `debug.disable_cursor_plane` — goes back into the frame, in the same
  commit that takes the plane off; a display the pointer is not on has its plane off. The outer keeps each
  session's cursor per display and shows only the fronted session's, over that session's own
  frames: the plane is off over the plate, the splash and every fade, and comes back with the
  frame that ends the fade. Two buffers, so a new image is never written into the one scanned
  out. `HOTSPOT_X/Y` are set where the plane has them (virtio-gpu shows our cursor as the host's
  pointer). Measured on the harness (2026-10-15, sampling the planes' state every 2-10 ms): pointer
  moves over a still desktop leave the primary plane untouched while the cursor plane follows; a
  window dragged by 120 moves at 16 ms flips 121 frames, cursor in step; a shape change (arrow to
  I-beam and back) lands image and position together.
- **Shutdown**: on logind's `PrepareForShutdown(true)` the outer stops following the seat and
  brings the shutdown splash in over what is on screen, the way the greeter arrives at a logout
  (a curtain coming down, or in depth; a fade from the plate). The splash then turns until the
  power goes (`boot-splash.md` §2.3). Every connected display shows it, as plymouth's would: one
  the session had blanked comes on for it and fades in from black; `false` brings back the session logind fronts. From that signal no
  session is fronted: none of their frames — new, or held for a switch back — reaches the screen
  again, except through the fade itself, whose `from` stays live: each frame the outgoing session
  presents while it ends replaces it (answered by the first blend that shows it, as for `to`), so
  its end-session dialog finishes closing under the fade. A fade from or to the splash takes the
  splash live too, redrawn in place in one picture per display as its spinner turns, so the
  spinner never stands still on screen, at boot or at shutdown. Measured on the harness
  (2026-10-16, `reboot.sh`): the curtain arrives in 300 ms at 8 ms frames, spinner turning, then
  the splash alone until the reboot; from a blanked display, it is lit and the splash fades in.
- **Harness** (`scripts/harness/`): drivers that run a scenario on the Limina harness VM while
  Limina records every frame that reaches its screen (`shutdown.sh`, `reboot.sh`, `login.sh`
  through the greeter, `switch.sh` between two running sessions, `logout.sh` through the
  end-session dialog, `switch-user.sh` through the power menu's Switch User; `deploy.sh` installs
  a build), and `report.py`, which reads a run back against the outer's flight recorder
  (the `synoik::system_compositor::flight` debug target: every flip that is not a session's steady
  frame, every step of a fade, and which virtio-gpu resource backs each framebuffer). Frames are
  named by that resource, not by time; each boot's journal is put on the capture's clock through
  `clock.py` and then fitted to the frames, since a guest's clocks are slewed by up to half a
  second early in a boot. The report gives a timeline of frames, the driver's steps and the
  journal; each fade's alpha fitted from the pixels (against a still taken as the run starts,
  when the screen presented nothing before the fade); the tail before power-off; a filmstrip.
- **Chord** (`system_compositor::chord`): on a seat without VTs the outer reads every evdev node
  itself (the `input` group, nothing grabbed) for Ctrl+Alt+F<n>, which activates the n-th seat0
  session in login order through logind's `ActivateSession` — off the event loop, allowed by
  `resources/synoik-system.rules` (`chvt` only). With VTs the session compositor already turns
  that chord into a VT switch, so the outer leaves it alone; `SYNOIK_SYSTEM_CHORDS=1`/`0`
  overrides the detection.
- **Inner** (`backend::nested`, selected by `SYNOIK_SYSTEM_SOCKET`): input exactly as the tty
  backend takes it; renders like headless into three scanout slots, only while logind has it
  active, its
  first frame waiting for the wallpaper; paces off `Presented` as the tty does off vblank. On losing the outer it keeps running,
  retries, re-registers its buffers and redraws in full.

Divergence from §5 step 6: the in-animation is gated on the incoming frame, but the outgoing
picture is not held indefinitely for it. After 300 ms without one the switch fades to the incoming
session's last answered frame, or the plate — logind has already moved input, and the previous
user's screen must not stay in front of whoever is at the seat now.

Measured 2026-10-11 on enhanced F44 under Limina (venus), sessions autologged on tty1 and tty2,
no fbcon: injected input reaches the fronted session only; SIGKILL of the outer is invisible on
screen (the stored fd is still master, the session reattaches within the 0.5 s retry); SIGKILL of
a session leaves its last frame up until its next. Ctrl+Alt+F2/F1 switches crossfade: fitting
each captured frame as `A + a·(B − A)` gives `a` on the ease-out curve (0.13 at 10 ms, 0.83 at
110 ms, 1.0 by 211 ms) with a mean residual under 0.3/255, then the session's own buffers take
over with no visible step; reversing at `a` = 0.46 continues from 0.46. With the fronted session
SIGSTOPped, the outer's chord (forced on) fronted the other session 40 ms after the keypress;
without the polkit rule logind refuses it (`InteractiveAuthorizationRequired`).

Measured 2026-10-11 on the same guest booted with a `CONFIG_VT=n` kernel: no `tty0`, no
`/dev/fb*`, seat0 `CanTTY=no`. Two sessions opened through PAM (`login`) with `XDG_SEAT=seat0` and
no TTY — a system unit standing in for the greeter — are both seat0 user sessions, the later one
`Active`. The outer turned the chord on by itself (6 evdev nodes) and it is the only switch path:
Ctrl+Alt+F1 fronted session 1 31 ms after the keypress. Super reached only the fronted session,
and switching away and back found its overview intact. The crossfade fits as on the VT guest
(`a` = 0.20 at 15 ms, 0.51 at 50 ms, 0.80 at 100 ms, 1.0 by 223 ms; residual under 0.25/255), the
session's own buffer following. Neither inner logged a warning, though each also sees the chord.

Not built: client buffers on overlay planes through the outer (the inner composites every
window); §3's atomic test-and-revert and the HDR, VRR and orientation fields; VRR and tearing
passthrough; a modeset under black (§4.2: a changed display cuts through its plate); custom
modelines nested; the inner asking the outer for a switch over IPC (§2.1's normal path). The boot
and shutdown splash (§6) is built: `boot-splash.md`.

## 11. Next

In order:

1. **The outer's animations on the harness** (§10 Harness): every transition has a driver, and the
   curtain and the depth are built to be tried side by side. Next, what trying them shows, and the
   handoffs between inner and outer: a login holds the greeter still for the session's first frame
   (about 0.45 s on the harness), and a logout holds the session's last frame until the greeter's
   first.
2. **No stalled frame between the initramfs splash and the outer** (`boot-splash.md` §6): the
   CPU splash keeps the spinner turning until the outer can draw.
3. **Overlay planes through the outer**: a client's buffer scanned out on an overlay plane, which
   needs the buffer forwarded across the wire and §3's atomic test-and-revert first.
