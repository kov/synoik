# A more real test harness

Research note, 2026-08-01. **No code changes proposed here yet** — this is the survey plus a
staged plan to argue about. Everything below was probed on this machine or read out of the
reference checkouts; citations are inline.

## 1. The gap

`cargo test --workspace` runs `BackendMode::HeadlessTest`, which by construction skips the
things a real session does. Two bugs found *outside* the suite pin the two halves of the gap:

- **Ordering the suite cannot reach.** The app-grid icon prewarm ran from `add_output`, which on
  the TTY path is reached synchronously from `Tty::init` → `connector_connected` →
  `synoik.add_output` (`src/backend/tty.rs:1524`) — tens of lines before the decode worker is
  spawned, so the warm silently no-op'd. Headless never runs that sequence. Worse, the fix
  landed inside `if mode != BackendMode::HeadlessTest` (`src/synoik.rs:1251`, call at `:1346`), so
  no test executes it in either direction. No test anywhere constructs a `Tty`: `Tty::new` has
  exactly one caller, `State::new` (`src/synoik.rs:1228`).
- **Pixels the suite cannot trust.** GPU-rendering clients composite empty under `--headless`
  (see `[[dmabuf-clients-blank-headless]]`), so window *contents* can never be judged from a
  headless shot — the exact case where a screenshot test would be most valuable.

Both are the same shape: the suite tests a compositor that runs a code path production never
runs, and skips the one it does.

## 1b. The cheap tier: fake the model input, not the hardware

Before building a mock at the D-Bus or kernel level, check whether the state can be injected at
**our own model** instead. `synoik msg action debug-set-battery` fakes a `BatteryStatus` where the
UPower watcher would have delivered one; the panel, the corpus and the live session then all
exercise the real path from the model onward.

That is a fraction of the cost of mocking UPower, and it buys the two things that actually matter:
every reading becomes reachable on hardware that will only ever report one of them, and the same
injection drives a headless corpus test (`the_battery_indicator_reads_every_power_state`).

It does not replace the tiers below — it cannot test our *reading* of UPower, only what we do with
what we read, so the parse stays unit-tested against real enum values. But for anything downstream
of a plain-data model, this is the first thing to reach for. See `RUNNING.md`
("Faking hardware states") for the convention.

## 2. What this machine already has

Probed 2026-08-01 on `7.1.5-limina16k`, aarch64:

| Thing | Status |
| --- | --- |
| `vkms` module | present, **not loaded** (`/lib/modules/…/vkms.ko`) |
| vkms configfs | **yes** — `configfs_register_subsystem`, `is_configfs_registered` in the module |
| vkms params | `create_default_dev`, `enable_writeback`, `enable_cursor`, `enable_overlay`, `enable_plane_pipeline` |
| `vng` / virtme-ng | installed (`/usr/bin/vng`) |
| python-dbusmock | 0.38.0 |
| podman / docker / bwrap / systemd-nspawn / unshare | all installed |
| `dbus-run-session`, `dbus-daemon` | installed |
| user namespaces | available (`max_user_namespaces=97121`) |
| libseat | 0.9.3 |
| `seatd` | **not installed**, available in Fedora repo |
| `umockdev` | **missing** (no binary, no GI typelib) |
| `/dev/kvm` | **absent** — `systemd-detect-virt` says `vm-other` |
| UML | not viable — aarch64, UML is x86/x86_64 upstream |

Two entries decide most of the design: **no `/dev/kvm`** (so any nested VM is TCG emulation,
~10-20x slow — nightly at best, never a dev loop), and **vkms has configfs + writeback**.

## 3. Mutter already built this — read it before inventing

`~/Projects/mutter/src/tests/kvm/` is a complete working harness for exactly our problem.
Its README, `virtme-run.sh`, `run-kvm-test.sh` and `install-udev-rules.sh` describe:

1. `vng` boots a purpose-built kernel (`kernel-version.txt` pins a `drm-next` tag,
   `build-linux.sh` enables `CONFIG_DRM_VKMS`) with the host filesystem exposed.
2. Inside, `meta-dbus-runner.py` / `mutter_dbusrunner.py` stands up **private session and system
   buses** and populates the system bus with python-dbusmock templates:
   `logind`, `localed`, `colord`, `gsd-color`, `rtkit`, `screensaver`.
3. `install-udev-rules.sh` assigns test devices to a **separate seat** via
   `ENV{ID_SEAT}="meta-test-seat0"`.
4. `mutter_dbusrunner.py` isolates `HOME`, `TMPDIR`, `XDG_CACHE_HOME`, `XDG_CONFIG_HOME`,
   `XDG_DATA_HOME`, `XDG_RUNTIME_DIR` into a temp root, and forces `GSETTINGS_BACKEND=memory`.
5. Tests run under `umockdev-wrapper` by default.

Mutter identifies its own test device by devpath prefix `/devices/faux/vkms/drm/card` or
`ID_PATH == "platform-vkms"` (`src/backends/meta-udev.c:126`), and has udev tags
`mutter-device-ignore` / `mutter-device-preferred-primary` (`meta-udev.c:110-145`).

**The VM is a CI constraint, not a requirement.** Mutter needs `vng` because GitLab runners
can't `modprobe`. We have passwordless sudo on a dev VM — so for us the VM is the *fallback*,
not the foundation. That inverts the cost structure in our favour.

## 4. The unlock: a mocked logind hands out *real* file descriptors

The single most useful thing in that tree is `dbusmock-templates/logind.py`. Its
`Login1Session.TakeDevice` has an `open_file_direct` fallback: it resolves major:minor through
`/sys/dev/char/<maj>:<min>/uevent`, `os.open`s `/dev/<devname>`, and returns a `dbus.types.UnixFd`.
`TakeControl` is a no-op when there's no host session.

Consequence for us: `LibSeatSession::new()` (`src/backend/tty.rs:412`) talks the logind protocol.
Point it at a private system bus carrying that mock and **the real `Tty` backend initialises with
no root, no seatd, and no VM** — against whatever device nodes we choose to expose. The template
also supports *passthrough* mode (forwarding `TakeDevice`/`TakeControl` to the host logind when
`host_system_bus_address` is set), which is the mode to use when we do want a real seat.

That kills the blocker from the earlier analysis. The reason a VKMS harness looked expensive was
"VKMS gives you a DRM device but not a session" — `logind.py` *is* the session, and it's ~200
lines of Python we can adapt. (It's LGPL-3.0-or-later, so GPL-3.0-compatible; it's plain
`dbus-python`, no GObject, so it doesn't collide with the no-GObject tenet.)

## 5. Proposed architecture — four tiers, not one harness

The mistake would be to replace the fast suite. Keep it; add tiers above it, each buying one
specific kind of realism, each independently runnable.

**Tier 0 — `HeadlessTest` (today).** ~15s, hermetic, no devices. Stays the dev loop and the home
of the conformance corpus. Unchanged.

**Tier 1 — mocked-system-bus, still headless.** A private session+system bus with dbusmock
templates for logind, UPower, NetworkManager, gsd, MPRIS, and friends; isolated `XDG_*`/`HOME`;
`GSETTINGS_BACKEND=memory`. Buys: real D-Bus round trips for the port's most D-Bus-heavy
surfaces (quick settings, lock screen, media/MPRIS, a11y, localed), tested against a *server we
don't control* rather than an in-process fake. Costs seconds, needs no privilege, and it directly
addresses `[[test-the-code-not-a-reimplementation]]` — the current risk is fakes that can't fail
for the mistakes they exist to catch. This tier alone is probably the best value in the whole
document, and it needs no vkms at all.

Also the natural home for a fix to a known hazard: relocatable folder stores clobbering real
dconf from a test (`[[app-folders-port]]`). Mutter's temp-root isolation is the answer.

**Tier 2 — real `Tty` backend on vkms, local, seat-isolated.** `modprobe vkms
create_default_dev=0`, create devices from configfs per test, run the compositor under the Tier 1
mocked logind. Buys: the entire `Tty::init` → `device_added` → `connector_connected` →
`add_output` sequence — hotplug, mode setting, output add/remove ordering, and the class of bug
in §1. Plus vkms **writeback**, which reads back what was actually scanned out: the first true
pixel oracle this project would have, and the answer to `[[dmabuf-clients-blank-headless]]`.

**Tier 3 — `vng` VM, nightly.** Mutter's approach verbatim, for anything Tier 2 can't do
(a kernel we don't run, module params that need a reboot, destructive DRM states). On aarch64
without KVM this is TCG and slow — accept that and run it nightly, not per-commit.

## 6. Risks, in the order they'll bite

**Perturbing kov's seat0 — the one that must not happen.** A new DRM card appears to *every*
DRM consumer, and mutter/gnome-shell on seat0 would happily pick up a vkms device as another GPU.
Mitigations, all needed: (a) `create_default_dev=0` so `modprobe` alone creates nothing;
(b) install the udev rule assigning `ID_SEAT` to a test seat **before** creating any device via
configfs; (c) verify with `loginctl seat-status` that seat0 did not gain the device. Mutter's
`ENV{ID_SEAT}="meta-test-seat0"` is precedent, and mutter's own `mutter-device-ignore` tag shows
the reverse direction exists too. **This needs a green light before the first `modprobe` —
7 live sessions are on this box, including gdm and kov's.**

**vkms has no render node.** It is display-only. `Tty::new` derives `primary_render_node` from
the primary node (`src/backend/tty.rs:458-476`) and hands it straight to
`VulkanRenderer::for_drm_render_node` (`:496`). With vkms as the primary node there is no
`NodeType::Render` sibling, so this either fails or must be split: vkms for KMS, `renderD128`
(virtio-gpu) for rendering. `primary_node_from_config` suggests the seam already exists, but this
is the **main unvalidated assumption in the document** and should be the first thing spiked —
it decides whether Tier 2 is a week or a month.

**Mocked logind is not logind.** Anything depending on real session lifecycle (activation
switches, idle/suspend inhibitors — i.e. the lock-screen port) may need passthrough mode against
the host bus, which reintroduces the isolation problem. Worth knowing which of the two modes each
test wants, up front.

**Silent green.** The sharpest trap, and the reason to build the tiers in order: `Tty::init`
early-returns when the session is inactive (`src/backend/tty.rs:546-549`), adding devices later
via `ActivateSession` — i.e. *after* `State::new` finishes, which is precisely the ordering where
the §1 bug does not reproduce. A half-configured Tier 2 harness passes for the wrong reason.
Any new tier must first be proven to **fail** on a known-bad commit before it is trusted;
`[[frame-log-instrument-confound]]` is the same lesson from the perf side.

**Missing pieces to install:** `seatd` (repo), `umockdev` (mutter runs tests under it by default).

## 7. Suggested order

1. Spike the vkms render-node split — cheapest way to de-risk the expensive tier. Read-only:
   check whether `primary_node_from_config` can point KMS at vkms and rendering at `renderD128`.
2. Build Tier 1. No privilege, no kernel modules, immediate value, and it's the substrate Tier 2
   needs anyway. Port `logind.py` and the temp-root isolation; add our own templates as the port
   needs them.
3. Prove Tier 1 fails on a known-bad commit before trusting it.
4. Ask about seat isolation, then Tier 2.
5. Tier 3 only if something demands it.

Independently of all of the above and worth doing regardless: move the app-grid prewarm so that
"worker exists ⇒ warm happened" is an invariant of the call site rather than an ordering to test,
and move the call outside the `HeadlessTest` gate so both modes execute the same code. A harness
that catches the bug is worth less than a shape that cannot express it.

## 8. Open questions

- Green light to `modprobe vkms` on this box, given seat0 has live gdm/kov sessions?
- Is Tier 1 worth landing on its own merits even if Tier 2 never happens? (My read: yes,
  comfortably.)
- Do we want the vkms writeback pixel oracle to replace or complement the existing Vulkan
  render-test snapshots?
