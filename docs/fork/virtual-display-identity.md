# Virtual display identity: what the VMM would have to tell us

*Written 2026-07-28, after three display-configuration bugs in a row that were all really one bug:
this VM's two displays are indistinguishable to the guest except by the modes they advertise.*

The compositor runs in a krun VM on an Apple laptop. Moving the VM's window between the laptop
panel and an external monitor changes what the guest's single `Virtual-1` connector advertises —
and nothing else. This document records exactly what the guest can see today, which parts of
GNOME's display-configuration model that breaks, what we did to cope, what we tried and reverted,
and what an ideal VMM would present instead. The last part is deliberately written so it would help
**stock mutter** too: every ask below is a standard EDID/DRM behaviour, not a bespoke protocol for
this fork.

**The conclusion, up front:** per-display settings cannot be made to work from the guest side. We
took the compositor-side fixes that are mutter's own behaviour anyway, reverted the one that was a
divergence, and the rest is a VMM ask — ideally EDID passthrough (§4.2).

## 1. What the guest sees today

`/sys/class/drm/card0/card0-Virtual-1/edid`, 128 bytes, decoded:

| Field | Value | Comment |
| --- | --- | --- |
| Manufacturer | `RHT` (0x4914) | Red Hat, the emulated device's vendor |
| Product code | `0x0001` | **constant** |
| Serial (header) | `1` | **constant** |
| Serial descriptor (`0xff`) | absent | |
| Monitor name (`0xfc`) | `krun-display` | **constant** |
| Manufacture date | week 30, 2025 | constant |
| EDID version | 1.4 | |
| Video input (byte 20) | `0x00` | **analog** — a virtual panel should say digital |
| Physical size | 17 cm × 11 cm | **constant, and not any real display** |
| Chromaticity | all zero | |
| Established timings | none | |
| Standard timings | 1920x1200, 1856x1160, 1680x1050, 1600x1000, 1440x900, 1400x875, 1280x800, 800x500 | the only per-display signal |
| Detailed timing #1 | 2048x1330 @ 215.90 MHz | the preferred mode — the host window's current size |

Everything a display-configuration store keys on — connector, vendor, product, serial — is a
constant. The only thing that changes when the VM moves to another display is the detailed timing
(the preferred mode) and the list of standard timings.

Two further consequences of shipping *standard* timings instead of detailed ones: the kernel
**generates** those modes with CVT, so their refresh rates come out as 59.885 / 60.972 / 62.940
rather than round numbers, and they are only as stable as the CVT implementation that produced
them. Any store that records a saved mode's rate has to compare with a tolerance (mutter uses
0.001 Hz, `meta-monitor.c` `MAXIMUM_REFRESH_RATE_DIFF`; we match it).

## 2. What that breaks

### 2.1 The scale guess is computed from a fiction

GNOME derives a monitor's default scale from its physical size (`meta-monitor.c`
`calculate_scale`, lines 2354-2400: DPI from the diagonal in inches, target 135 for panels under
20", pick the nearest supported scale). We ported that verbatim (`src/utils/scale.rs`).

With the EDID's 17×11 cm, the 2048x1330 preferred mode computes to ~306 DPI and picks **2.25**.
That is the 225% Gustavo kept landing on.

For a real 14-16" laptop panel the same mode computes to 152-175 DPI and picks **1.25** — which is
exactly the scale he set by hand. *An honest physical size would have made the guess right with no
saved configuration at all.* This is the single highest-value fix on the list.

(mutter also bails to 1.0 when the EDID encodes an aspect ratio instead of a size,
`meta_monitor_has_aspect_as_size`; our port only checks for zero. Worth adding if a VMM ever
reports 16:10 as "16 cm × 10 cm".)

### 2.2 A saved configuration can only remember one display

mutter keys a stored configuration on the set of `MetaMonitorSpec`s it covers — `{connector,
vendor, product, serial}` (`meta-monitor-private.h:30-35`) — and replaces the entry with the
matching key on save (`meta-monitor-config-store.c`, `g_hash_table_replace`). With one spec for
both displays, saving settings for the external monitor **forgets** the laptop panel's, and vice
versa. A real GNOME session on this VM would behave the same way.

### 2.3 Nothing announces that the display changed

The connector never disconnects; its mode list is simply different the next time it is read. So
there is no hotplug moment at which a compositor would naturally re-run its configuration chain,
and every piece of per-display state it holds (the applied scale, the current mode) silently
carries over to a display it was never chosen for.

## 3. What we do today to cope

Three commits, all of them working around section 2:

- `ce9325c1` — a saved scale is only applicable **at the mode it was saved for**. This is mutter's
  own rule (a stored config whose mode can't be assigned is rejected), and here it doubles as the
  only way to tell the two displays apart.
- `fe61c6dd` — restore the saved **mode** as well as the scale, since the gate above means a
  monitor that comes up at its preferred mode never matches its own saved entry
  (`MonitorsConfig::saved_modes_for` → `tty::target_mode`).
- `fd001ae6` — a live-applied config dies with the display it was applied to: if the connector no
  longer offers the mode the apply named, the override and the fields it wrote are cleared, and the
  chain re-runs. Mutter's `is_config_applicable`, applied to a hardware change it can't see.

All three are mutter's own rules; none of them is a divergence.

### Tried and reverted: remembering both displays at once

A fourth change (`10a4fd32`, reverted the same day) went further: **merge** saves into
`monitors.xml` keyed on connector *and mode* instead of the monitorspec, so the laptop panel's
settings and the external monitor's could coexist under one identity. It worked for the case it was
written for, and it does not work in general — **the internal panel's preferred mode, 2048x1330, is
also advertised by the external monitor**. Once two displays share a mode, a mode-keyed store
cannot say which entry belongs to which, and the tie-break (most recently saved) is a guess that
will be wrong half the time. Reverted rather than shipped: a store that silently applies another
display's scale is worse than one that guesses from DPI.

So there is no per-display memory here, and there cannot be one built on this EDID. The scale a
display gets is the one saved for whatever mode it comes up in, else the DPI guess — and both are
computed from a physical size that is a constant. **This is the piece that has to be fixed on the
VMM side**; §4.1 and §4.2 are not conveniences, they are the whole feature.

## 4. The ideal world

Ranked by value, each with what it buys a stock mutter. §4.2 is the one that actually settles this
— §4.1 is the cheap half of it, worth doing on its own if passthrough is hard.

### 4.1 Report the host display's real physical size

Take the physical dimensions of the host display the VM's window is on
and put them in the EDID (bytes 21-22, and the detailed timing's mm fields).

*Stock mutter:* its computed default scale becomes right on the first login, for every guest OS
that follows the EDID — which is all of them. No configuration, no store, no user interaction.

If the window is genuinely floating on the host and no single display owns it, reporting the
display it mostly covers is still far better than a constant.

### 4.2 Pass the host display's own EDID through

The preferred form, and the one that needs no invention: when the VM's window is on a given host
display, hand the guest **that display's EDID**, the way GPU passthrough does. Every physical
monitor then arrives with the identity its manufacturer gave it — including its real physical size,
so this subsumes §4.1 — and two different external monitors are two different monitors rather than
both being `Virtual-2`.

Failing that, at minimum vary the product code and serial number (and ideally the `0xfc` monitor
name) per host display. The requirement is *stability*: the same physical display must produce the
same identity across VM restarts, or the store fills with entries nothing can match.

*Stock mutter:* stored configurations key correctly, so per-display scale/resolution/layout is
remembered exactly as on bare metal, and `gnome-control-center`'s display list becomes readable.
For us it is the difference between having per-display settings and not having them at all —
§3 explains why no amount of compositor-side cleverness substitutes.

### 4.3 Announce the change as a hotplug

When the window moves to a display with a different identity or mode list, deliver a DRM hotplug
uevent — ideally connector-disconnected then connector-connected, so the guest tears down and
rebuilds its output rather than mutating one in place.

*Stock mutter:* `meta_monitor_manager_reload` → `ensure_configured` runs, which is the code path
that consults the store and computes a default. That is exactly the moment a compositor is designed
to reconsider a display, and it is where all the logic already lives.

### 4.4 Expose one connector per host display

If the host has two displays attached, present two connectors and mark the one the window is on as
connected. Then moving the window is an ordinary monitor hotplug, layouts can be saved for each,
and the "same connector, different panel" case disappears entirely.

### 4.5 Ship detailed timings, and mark the input digital

Emit the advertised resolutions as detailed timing descriptors (or at least a stable, documented
set) rather than leaving the kernel to CVT them, so refresh rates don't drift between kernel
versions and a saved mode keeps matching. Set byte 20's high bit — a virtual panel is a digital
one, and some code paths do check.

### 4.6 What is *not* needed

A scale hint channel. It is tempting to have the VMM tell the guest "use 125%", but there is no
such thing in EDID or DRM, no guest OS would consume it without new code, and it is unnecessary:
an honest physical size plus a distinct identity gets both a right first guess (4.1) and a
remembered user choice (4.2). Asking for a new protocol would make this fork's needs special;
asking for a correct EDID makes every guest better.

## 5. Verifying any of this

- Dump what the guest sees: `sudo cat /sys/class/drm/card0/card0-Virtual-1/edid | od -A d -t x1`,
  or `edid-decode` if installed.
- What we resolved it to: `niri msg outputs` prints physical size, current mode, the advertised
  mode list and the scale in force.
- The decisions that consume all this are pure functions with unit tests —
  `mode_is_available` / `applied_config_is_stale` / `choose_target_mode` in `src/backend/tty.rs`,
  and `MonitorsConfig` in `src/monitors_xml.rs`. The DRM plumbing around them is not tested: doing
  that properly needs a VKMS device whose connectors and mode lists can be changed from configfs,
  which is the one piece of coverage still missing here.

## 6. VMM-side response (2026-07-30, initial assessment)

Read and agreed — this is a well-shaped ask, and §4's framing ("a correct EDID, not a new
protocol") is exactly the design we want too: everything below benefits a stock guest with no
guest components, which is a hard requirement on our side anyway. Point-by-point:

**Accepted, first delivery — §4.1 + §4.5 + the stable-identity floor of §4.2.** The EDID the
guest sees is generated in the VMM's virtio-gpu layer, which we own and patch routinely; the
host process already knows which physical display the window is on (it re-fits the guest mode
on display migration today). The plan is one mechanism patch (per-scanout EDID set/update API
in the virtio-gpu device) plus host-side policy: real physical size from the host display
(bytes 21-22 + detailed-timing mm), a stable per-display identity synthesized from the host's
vendor/model/serial (product code, serial, `0xfc` name — hashing the display name when a
monitor reports serial 0), detailed timing descriptors for the advertised modes instead of
CVT-able standard timings, and the digital input bit. Your §2.1 scale-guess bug is also *our*
bug — the fictional 17×11 cm makes our own benchmark rigs come up at 2.25 — so this has
independent priority on our side.

**§4.2 verbatim (raw host EDID passthrough) — partial, honestly.** macOS on Apple Silicon does
not reliably expose a display's raw EDID (the built-in panel in particular; external monitors
sometimes, via IOKit). So the *guaranteed* form is the synthesized-stable identity above,
built from the identity fields macOS does expose — which your fallback paragraph explicitly
blesses ("the requirement is stability"). Where the OS hands us a real EDID we can pass it
through opportunistically, but don't design against it being present.

**§4.3 (hotplug) — accepted, one verification owed before we promise semantics.** virtio-gpu
has a display-changed event; the guest kernel driver fires a DRM hotplug event on it. What we
still have to verify empirically is that a *stock* guest kernel re-reads the EDID at that
moment and that the uevent shape (connector change vs disconnect→connect) matches what
`meta_monitor_manager_reload` wants. If in-place identity mutation confuses guests, we can
model disconnect→connect by dropping and re-raising the scanout. We'll report what the stock
kernel actually does rather than promise the ideal form up front.

**§4.4 (connector per host display) — agreed as the destination, not the first step.** This is
effectively the multi-display feature and rides that work. The EDID plumbing in the first
delivery is per-scanout from the start so §4.4 slots in without rework. Note the interim
consequence: until then, a window migrating displays remains an identity change *on one
connector* — which is precisely why §4.3's hotplug moment has to come with it.

**§4.6 — fully agreed.** No scale-hint channel; an honest EDID is the whole contract.

Sequencing: (1) EDID honesty — size/identity/timings/digital, one VMM release; (2) hotplug on
migration, after the stock-kernel verification; (3) connector-per-display with multi-display.
For (1) the acceptance test is your §2.1 case: fresh guest, no monitors.xml, window on the
laptop panel → first-login scale 1.25, and two different external monitors appearing as two
distinct entries in the store.

*— the VMM side.*

### 6.1 Addendum (2026-07-30): overlay planes and VRR on the roadmap

Related, and going a bit beyond the EDID asks: we intend to add **overlay plane** support and
**VRR** to the virtual display, so a guest on a ProMotion host panel (the MacBook internal
screen: 24-120 Hz adaptive) can actually use it. Sketch, not yet designed:

- **VRR**: advertise adaptive sync in the EDID/connector caps (a range descriptor fits
  naturally in the §4.1/§4.5 EDID work — another reason to emit detailed descriptors
  ourselves), accept `VRR_ENABLED`-style state on the CRTC, and map guest flips onto the
  host's adaptive cadence instead of a fixed 60 Hz tick. The §29/§30 present-timestamp work
  is a prerequisite in spirit: honest per-flip present feedback is what makes guest-side VRR
  scheduling meaningful.
- **Overlay planes**: expose one or more overlay planes on the virtual CRTC so the compositor
  can put the cursor (and eventually fullscreen video/direct scanout candidates) on a plane
  instead of compositing it — host-side these map cheaply onto separate CALayers, which is
  also how we'd sidestep a whole class of full-frame damage for cursor-only updates.

Both are guest-visible as bog-standard KMS features (stock mutter consumes either without new
code), same design language as the rest of §4. We'll write these up properly in our tree and
report the plan here.

*— the VMM side.*
