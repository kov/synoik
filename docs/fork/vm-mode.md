<!-- SPDX-License-Identifier: GPL-3.0-only -->

# VM mode — a sketch

Not implemented. This is the design we agreed on 2026-08-13, written down so the shape is settled
before anyone starts.

## What it is

A session-level **policy mode** that overrides GNOME's settings-derived policy in the places where
running inside a virtual machine makes GNOME's default the wrong answer. It is not a debug switch
and not a second configuration system: it is one input to the model that already computes policy.

The motivating case is the screen lock. On a VM the guest's scanout buffer is what a host snapshot
captures, so an automatic lock on suspend replaces the picture of the desktop with a lock curtain;
and authenticating to a guest whose disk the host can read unencrypted is theatre. Both are correct
GNOME behavior on hardware and wrong here.

## The rule that decides what belongs in it

**VM mode suppresses policy that fires without the user asking. It never changes what a user-initiated
action does.**

So the suspend lock and the idle lock are in scope, and `org.gnome.ScreenSaver.Lock`, logind's `Lock`
signal (`loginctl lock-session`) and the `Lock` keybinding are *not* — those are someone asking for a
locked screen, and they must keep working exactly as they do now. This line is the whole reason the
mode is safe to turn on by default: the escape hatch is always the explicit action.

It also never *weakens* an administrator's veto. `org.gnome.desktop.lockdown disable-lock-screen`
only ever forbids locking, and VM mode only ever suppresses it, so the two cannot disagree.

## How it turns on

Tri-state, defaulting to autodetect:

- **auto** (default) — on when we are in a VM.
- **forced on / forced off** — an explicit override, for a VM that wants hardware behavior or a
  developer reproducing VM behavior on metal.

Detection: `org.freedesktop.systemd1.Manager.Virtualization`, a string on the system bus that is
empty on bare metal and names the hypervisor otherwise (`"vm-other"` on kov's libkrun VM). We already
hold a system-bus connection for login1/UPower/NetworkManager, so this costs one property read at
startup and no subprocess. `/sys/class/dmi/id/sys_vendor` is the fallback if that ever proves
unavailable; do **not** infer it from the renderer (venus present ≠ virtualized, GPU passthrough
exists).

The override belongs with the other environment switches (`SYNOIK_VK_VALIDATION`, `SYNOIK_DEBUG_*`) —
there is no config file. Note that on a lingering user `environment.d` is a dead drop; a systemd unit
override is the only thing that reaches the session.

## Where it lives

One field on the settings model, folded in at **the single place that already composes the policy
struct** — `GnomeSettings::load_*` in `src/gnome.rs`, where `ShieldSettings` is built from
`org.gnome.desktop.screensaver` and `org.gnome.desktop.lockdown` (`:621`, `:648`). Everything
downstream — `ScreenShield::prepare_for_sleep`, `on_session_idle`, `wants_sleep_inhibitor` — stays
exactly as it is and never learns the mode exists.

Two properties this placement buys, both of which are the point:

- **It overrides at read time; it never writes dconf.** Otherwise you cannot tell what the user asked
  for from what the mode did, and turning the mode off would not give the user their settings back.
- **The mode is visible in the inspectable model**, so "why didn't my screen lock" has an answer over
  synoik-ipc rather than in the source. A policy override nobody can observe is a bug report waiting
  to happen.

## First slice

Screen lock only, which is `lock_enabled = false` when the mode is on. Pinned by conformance tests
that assert both halves of the rule: `PrepareForSleep(true)` leaves the screen uncovered, and
`ScreenSaverToSynoik::Lock` still locks with a verifier.

Note that this makes the suspend path's inhibitor work moot in VM mode — `wants_sleep_inhibitor`
already returns false with locking off, so the suspend is not delayed at all. That is the desired
outcome, not something to special-case.

## Candidates for later, not decided

Each of these is a place where a VM is already known to want a different default. None is in the
first slice, and none should be added without the same "does a user action override it?" check:

- **Frame scheduling.** This VM cannot wake a thread on time (~1 ms floor, measured), which is what
  the deadline-dispatch margin dial exists for. A VM default for it is plausible.
- **Cursor.** The hardware cursor plane's hotspot handling is a virtio-gpu problem; the software
  cursor is the current interim, chosen globally rather than per-environment.
- **Per-display state.** `monitors.xml` and output-scaling memory are not solvable guest-side.

Explicitly **not** in scope: power policy (gsd owns auto-suspend), and anything that would make the
guest less like a GNOME session for its clients.

## Open questions

- Does the mode suppress the idle *blank* as well as the idle lock, or only the lock? The blank is
  not an authentication wall, so the snapshot argument is weaker; kov's session sidesteps it today
  with `idle-delay 0`.
- Per-machine or per-session? dconf is already per-machine, so the mode's value only has to be
  per-session if we want two sessions on one host to differ — no known reason to want that.
