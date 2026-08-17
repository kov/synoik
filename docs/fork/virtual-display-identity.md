# Virtual display identity: what the VMM has to tell us

The compositor runs in a VM on an Apple laptop, with **one** virtual connector (`Virtual-1`) whose
EDID describes whichever host display the VM's window currently sits on. Moving the window between
the laptop panel and an external monitor rewrites that EDID in place. This document records what
the guest can see, which parts of GNOME's display-configuration model that stresses, what the
compositor does about it, and what the VMM has to present for the rest to work. The asks are
deliberately written so they would help **stock mutter** too: each one is standard EDID/DRM
behaviour, not a bespoke protocol for this fork.

**The conclusion, up front:** per-display settings work exactly as well as the EDID's identity
fields are honest and distinct. Where a VMM varies vendor/product/serial per host display, the
compositor now remembers each display separately, re-reads the identity whenever the EDID changes,
and needs nothing further. Where the EDID is a constant (§1), no amount of compositor-side
cleverness substitutes (§3) — that is the VMM ask, ideally EDID passthrough (§4.2).

## 1. What a constant-identity EDID looks like

The krun images' EDID: `/sys/class/drm/card0/card0-Virtual-1/edid`, 128 bytes, decoded (a VMM that
delivers §4.1/§4.2 varies the marked fields per host display instead):

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

### 2.2 One connector carries two displays, so the store is asked to key on identity alone

mutter keys a stored configuration on the set of `MetaMonitorSpec`s it covers — `{connector,
vendor, product, serial}` (`meta-monitor-private.h:30-35`) — and replaces the entry with the
matching key on save (`meta-monitor-config-store.c`, `g_hash_table_replace`), writing every entry
back out. Sharing one connector between two displays therefore costs nothing *as long as the
identity fields differ*: two identities are two keys, two stanzas, two remembered configurations.
With a constant EDID they are one key, and saving the external monitor's settings overwrites the
laptop panel's. A real GNOME session on such a VM behaves the same way.

### 2.3 Nothing announces that the display changed

The connector never disconnects, and its mode list may not even differ — so no DRM event says "this
is a different panel now". A compositor that captures a connector's identity only when it goes
connected keeps reporting the previous display's vendor/product/serial, and every piece of
per-display state it holds (the applied scale, the current mode) silently carries over to a display
it was never chosen for.

## 3. What the compositor does

- **Every connected connector's EDID is re-read on every DRM device-changed event**, and an
  identity that changed is handled as the re-plug it really is: the applied config is dropped, the
  output is torn down, and `on_output_config_changed` rebuilds it through the whole configuration
  chain (`Tty::refresh_changed_identities`). This is mutter's `meta_monitor_manager_reload` →
  `ensure_configured`, and it is what makes an in-place EDID swap work without the VMM having to
  fake a connector cycle. A failed EDID read yields *no* identity rather than a new one, so it
  never counts as a change.
- **A save merges into `monitors.xml` instead of replacing it** (`monitors_xml::merge`): the stanza
  for this set of monitors is replaced, every other saved configuration is copied through as raw
  source text, so configuring one display keeps the others' settings — and fields we don't model
  (mutter's full-precision rates, anything a newer mutter adds) survive the round trip. Our key
  leaves `<vendor>` out and folds case, because mutter writes the raw PNP code and a lowercase
  `0x%08x` fallback serial where we write the decoded make and uppercase; keying on those bytes
  would file a second stanza for the display the user is configuring.
- **A saved scale is only applicable at the mode it was saved for** (`ce9325c1`), and the saved
  **mode** is restored along with it (`fe61c6dd`, `MonitorsConfig::saved_modes_for` →
  `tty::target_mode`) — otherwise a monitor that comes up at its preferred mode never matches its
  own entry. Both are mutter's rules; with a constant EDID the mode gate doubles as the only way to
  tell two displays apart.
- **A stored scale is applied even when it is off the mode's ladder** — an accepted divergence.
  mutter rejects one ("Scale %g not valid for resolution", meta-monitor-manager.c:2674) and falls
  back to its computed default; we keep the user's saved value. The scales a mode *offers* are
  mutter's (`utils::scale::supported_scales`), so nothing new lands off the ladder — only stanzas
  written before that ladder existed, and silently moving a display the user had already set is the
  worse outcome. Settings will not show such a scale as selected.
- **A live-applied config dies with the display it was applied to** (`fd001ae6`): if the connector
  no longer offers the mode the apply named, the override and the fields it wrote are cleared and
  the chain re-runs. Mutter's `is_config_applicable`, applied to a hardware change it can't see.

None of this is a divergence from mutter.

### Why identity cannot be replaced by the mode list

Keying the store on connector *and mode* instead of the monitorspec was tried (`10a4fd32`,
reverted the same day). It works until two displays advertise a mode in common — the krun internal
panel's preferred 2048x1330 is also advertised by the external monitor — and then the store cannot
say which entry belongs to which, with a most-recently-saved tie-break that is wrong half the time.
A store that silently applies another display's scale is worse than one that guesses from DPI.

**Require an observation, not an inference: a display's identity has to come from the EDID.** With
a constant EDID the scale a display gets is whatever was saved for the mode it comes up in, else the
DPI guess — and that guess is computed from a physical size that is also a constant. §4.1 and §4.2
are not conveniences, they are the whole feature.

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

### 4.3 Deliver a uevent whenever the EDID changes

When the window moves to a display with a different identity or mode list, deliver a DRM hotplug
uevent. **An in-place EDID swap on a connector that stays connected is enough** — we re-read every
connected connector's EDID on each device-changed event (§3) — but *some* uevent is required, since
a compositor has no other reason to look. A connector cycle (disconnected, then connected with the
new EDID) also works and needs no wall-clock gap between the two on our side; it is simply more
disruptive than the in-place swap.

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
- What we resolved it to: `synoik msg outputs` prints physical size, current mode, the advertised
  mode list and the scale in force.
- Read back what the store holds: `~/.config/monitors.xml` should carry one `<configuration>` per
  display that has ever been configured, and `GetCurrentState` (`gdbus call --session --dest
  org.gnome.Mutter.DisplayConfig --object-path /org/gnome/Mutter/DisplayConfig --method
  org.gnome.Mutter.DisplayConfig.GetCurrentState`) should report the identity of the display the
  window is on right now.
- The decisions that consume all this are pure functions with unit tests —
  `mode_is_available` / `applied_config_is_stale` / `choose_target_mode` in `src/backend/tty.rs`,
  and `MonitorsConfig` / `merge` in `src/monitors_xml.rs`. The DRM plumbing around them
  (`refresh_changed_identities` included) is not tested: doing that properly needs a VKMS device
  whose connectors and mode lists can be changed from configfs, which is the one piece of coverage
  still missing here. Until then it is verified live, by moving the VM's window between two host
  displays and unplugging one. Measured 2026-08-17 on a two-display host (a 3840x2160 DELL P2723QE
  and a 2048x1328 built-in panel, distinct identities): each display kept its own scale across
  every move and across an unplug/replug, `monitors.xml` held one stanza per display, and one
  sample caught the connector carrying the *new identity while still advertising the old mode list*
  — which the compositor followed.

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
