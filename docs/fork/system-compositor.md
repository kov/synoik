# The system compositor

> Status: **design, agreed 2026-08-15.** Nothing implemented. The blocking unknown (§9 S1) is
> resolved: the outer is an unprivileged system user outside logind.

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
- the inner gets a third backend alongside `winit` and `tty`: **`nested-privileged`**.

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
- **Session switch** — a polkit rule grants the outer's user `org.freedesktop.login1.chvt`, the
  action logind checks for `ActivateSession`.
- **Its own input** — the `input` group, for the chord matcher (§2.1).

The inner's devices come from logind (§2), so there is no device-brokering helper either.

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
mode; one rule settles all of them.

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

Plymouth replacement is a clean later phase. When we take it on, it means:

- **Living in the initramfs.** Start there, hold the DRM fd, keep running across `switch_root` —
  processes survive the pivot. This is literally plymouth's mechanism, so it is known-viable.
  Costs: mesa + a Vulkan driver in the initrd is 100 MB+, and we run a stale binary until re-exec.
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

**S2 — Greeter as a client: confirm.** Assumed throughout: the greeter is a separate application,
a client of the outer, doing PAM and then asking the outer to spawn the session. Not yet decided
against the alternative (greeter mode *inside* the outer, as gnome-shell does with GDM). Note
`dreams-assessment.md` §9 already commits to building the greeter with the design-dreams grid
layout rather than porting GDM's.

**S3 — Suspend/resume and lid across the boundary.** Who owns the inhibitor, and does the shield
still ride the flip (cf. the "a suspend waits for a PRESENTED shield" finding) when the presenting
compositor is the outer and the locking one is the inner?

**S4 — Lock screen: inner or outer?** Putting it in the outer is the security win (unlock is not
inside the session, which is GDM's structural problem today). But we already implement a lock
screen in the inner, and moving it means the outer grows real UI — against §1.

**S5 — Multi-seat.** Not considered anywhere above.
