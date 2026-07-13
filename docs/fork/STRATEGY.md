# A Rust GNOME-flavored desktop: strategy

> Status: living strategy — now tracked in git (as of 2026-07). The foundations below are the
> original June-2026 exploratory pass; the **Decisions locked** block records what has since
> been committed to and where those decisions supersede the body.
> Grounded against locally checked-out **mutter 50.1** (`/home/kov/Projects/mutter`,
> ~610k LOC C) and **gnome-shell 50.1** (`/home/kov/Projects/gnome-shell`,
> ~83k LOC C incl. St ~57k, + ~81k LOC GJS), plus current (2025–26) Rust ecosystem,
> cosmic-comp v1.0, and niri.

**Decisions locked (2026-07-04):**
- **Hard fork — D1 settled = niri.** niri is the committed base; `main` is the only living
  branch; we no longer rebase or merge from niri upstream (see project `CLAUDE.md` §Git).
- **Own the render stack now**, reversing §3.10/§6's earlier "defer the ash/Vulkan renderer to
  the endgame." We hand-roll a bounded Vulkan renderer on `ash`, keeping the Smithay GLES
  backend live behind a flag during the port, for zero-copy on virtio-gpu Venus + deleting the
  zink GL-translation layer. **Stage 0 (standalone ash-on-Venus spike) is COMPLETE** — see the
  rewritten §3.10.
- **Panel — D2 settled = in-compositor.** The top panel landed in-compositor (opaque bar,
  Activities button toggling the overview, live clock, top strut).

---

## 0. Charter (read this first)

The honest finding from a deep feasibility pass *and* an adversarial red-team is:

- **As literally specified** — a one-to-few-person, slow, multi-year, *full drop-in
  gnome-shell replacement* that keeps GNOME's in-process Activities overview,
  bootstrapped from cosmic-comp, with a hand-built Rust St/Clutter toolkit, a custom
  text stack, and an extension API — **stacks 3–4 funded-team domains onto one person
  and dies in the "unusable valley."** It only becomes daily-usable years in. That is
  the classic solo-rewrite death.

- **Reframed** — *"my personal GNOME-flavored Rust Wayland desktop,"* daily-driven from
  week one, reusing much and building little, importing GNOME behaviors one at a time,
  each shippable the day it lands — **is sane, motivating, and genuinely enjoyable.**

This document adopts the reframed charter and treats the maximalist endgame
(GObject-free, Cogl-free, in-process CSS toolkit, own text stack, extension host) as the
**long arc** reached incrementally, never as a prerequisite for usefulness.

**Charter:**
1. A GNOME-*behaviors* Wayland compositor + shell, written in Rust, carried as a **fork**
   (no upstreaming obligation). License: **GPL-3.0-only** (see §9).
2. **Always usable.** No milestone lands unless it is daily-drivable that day. Start by
   daily-driving an existing Rust compositor and bending it toward GNOME, one change at a
   time.
3. **Reuse much, own little.** The endgame ("shed GObject/Cogl/Clutter/GJS") is an
   *aspiration reached last*, not a milestone you schedule early. `gio-rs`/`glib-rs` stay
   for D-Bus/platform plumbing indefinitely; only the rendering/toolkit layer is worth
   replacing first.
4. **Behavior-compatible, not bug-for-bug.** The spec is a *conformance corpus* (§8), not
   the old source tree. "Drop-in" is staged by contract (§3.8), not all-or-nothing.
5. **Extensions are deferred, tiered, and non-blocking** (§4). They never drive core
   architecture.

**Non-goals (initially):** bug-for-bug fidelity; full extension compatibility; HDR/color
management; a custom text stack; chasing GNOME upstream in lockstep; X11 session WM
(rootless XWayland only).

---

## 1. The bet, in one paragraph

The compositor/backend half of a modern GNOME is **already a solved problem in Rust**:
Smithay + `drm-rs`/`gbm`/`input`/`libseat`/`calloop` ships to users today via
cosmic-comp (GA, Dec 2025) and niri. So we do **not** rewrite mutter's KMS/DRM/Wayland
core — we adopt a proven Smithay-based compositor and spend our effort on (a) **GNOME
window-management *policy*** and (b) the **GNOME shell UX** (overview, panel, quick
settings, …), plus (c) the **GNOME D-Bus/GSettings contract surface** that makes it a
drop-in. The dominant remaining cost is the **in-process, CSS-themable UI toolkit**
(the St/Clutter replacement) — the single weakest link in the Rust ecosystem — which we
attack last, reuse around for as long as possible, and gate behind a decisive experiment.

---

## 2. The central tension, and the base decision

### 2.1 The spine of the whole project

GNOME's defining UX — the Activities overview — renders **live window thumbnails** as
transformable, effect-laden actors that morph from desktop position into a grid, with
video still playing in the thumbnail. In GNOME this is *zero-copy* and only possible
because the shell runs **in the compositor process**, cloning live `MetaWindowActor`s
(`js/ui/windowPreview.js`, `Clutter.Clone`).

Wayland has **no primitive to live-embed another client's surface**. The only
cross-client pixel path is compositor-mediated *capture* (`ext-image-copy-capture`),
which copies frames (a GPU blit + fence per thumbnail per damage event) and makes the
morph animation impossible. Therefore:

> **A GNOME-faithful overview must be rendered *inside the compositor*.** This is the one
> decision everything else hangs on.

**This is solved-in-principle in Rust today:** niri renders its overview in-compositor by
*rescaling the same live render elements* (`RescaleRenderElement<MonitorInnerRenderElement>`)
— no capture, fully live, you can even type into a window in the overview. So the *live
window* part is not the long pole. The long pole is the **CSS-themable chrome** drawn
around those windows (dash, app grid, search, panel) — i.e. the St/Clutter toolkit (§3.6).

### 2.2 cosmic-comp vs niri vs stock Smithay (the base)

You proposed bootstrapping from cosmic-comp. The research refines this:

| Base | For us | Against us |
|---|---|---|
| **niri** *(recommended primary base/reference)* | Single-process, **in-compositor UI** already; **GNOME-faithful in-compositor live overview**; **already implements GNOME/Mutter D-Bus** (`gnome_shell_introspect`, `gnome_shell_screenshot`, `mutter_display_config`, `mutter_screen_cast`, `mutter_service_channel`, `freedesktop a11y/login1/screensaver`); renders its own UI text with pango/pangocairo. Architecture is the closest to GNOME's. | Scrollable-tiling policy (to delete); smaller ecosystem than COSMIC. |
| **cosmic-comp** | Most polished/complete compositor; embedded-toolkit pattern (`IcedElement`); security-context gating; session-lock hosting; magnifier/zoom base; broad protocol coverage. | **Out-of-process shell model fights GNOME's**; ~32k LOC of opinionated tiling/stack/tab **policy to delete**; **no GNOME D-Bus** (DisplayConfig/Shell/ScreenCast absent); **no PipeWire**; config is RON, not GSettings; heavy git-pinned COSMIC dep web. Reusing cosmic-**comp** does **not** let you reuse cosmic's shell (separate layer-shell client crates). |
| **stock Smithay (anvil)** | Maximum freedom, minimal deletion. | You build the most. |

**Heuristic (from the red-team):** *pick the base by how much you DELETE, not by what
exists.* By that test **niri wins** for this goal: its *non-policy* infrastructure
(in-process UI, live-overview technique, GNOME D-Bus) is highly reusable; only its policy
is deletion. cosmic-comp's policy *and* process model are both deletion. **Mine
cosmic-comp** for the pieces niri lacks (embedded toolkit pattern, security-context
gating, magnifier base, capture→PipeWire path).

> **Open decision D1 (§10):** niri base vs cosmic-comp base vs stock Smithay. Recommend
> niri primary + cosmic-comp as a parts donor; settle it with Experiment 2 (§6).

---

## 3. Target architecture

### 3.1 Layering

```
                 ┌─────────────────────────────────────────────┐
   shell-ui      │ overview · panel · quick-settings · app-grid │  (in-process module)
   (Rust)        │ search · alt-tab · OSD · lock · notifications│
                 └───────────────┬──────────────────┬──────────┘
                                 │ ShellContext      │ st-toolkit
                                 │ (typed Rust API)  │ (CSS retained toolkit)
   control      ┌────────────────▼──────────────────▼──────────┐
   plane        │  Command → Event → State  (observable model)  │  §3.4
                └────────────────┬──────────────────────────────┘
   policy        │ GNOME WM policy: focus-stealing, placement,   │
   (Rust)        │ dynamic workspaces, tiling, ~112 keybindings  │  §3.3
                 ├──────────────────────────────────────────────┤
   contracts     │ zbus: org.gnome.Shell.* + org.gnome.Mutter.* │  §3.8
                 │ GSettings (org.gnome.shell / .mutter)         │
                 ├──────────────────────────────────────────────┤
   compositor    │ Smithay: DRM/KMS · GBM · libinput · libseat · │  §3.2  (KEEP)
   core (base)   │ calloop · GLES renderer · Wayland protocols · │
                 │ XWayland · explicit-sync · session-lock       │
                 └──────────────────────────────────────────────┘
   foundations:  text (parley/fontique/harfrust/swash)  ·  zbus  ·  AccessKit  ·  Vello
```

Distinct crates/repos in §5.

### 3.2 Compositor core — KEEP (from niri/cosmic-comp/Smithay)

Reuse as-is, do **not** reinvent: KMS/DRM (atomic), GBM, libinput, libseat session +
VT/suspend, the calloop loop, **Smithay's GLES renderer + damage tracking** (do *not* use
wgpu to composite untrusted client buffers — it hides dmabuf/explicit-sync/modifier
control), dmabuf + `drm_syncobj` explicit sync, fractional scale, viewporter,
presentation-time, **XWayland** (rootless only), layer-shell, session-lock hosting,
security-context, pointer constraints, tablet, text-input/input-method, idle.
This is the decade of hardware-quirk pain you get for free.

### 3.3 Window-management policy — BUILD (reproduce GNOME behaviors)

Replace the base compositor's policy layer with GNOME semantics, **reproduced to a
behavior spec** (not ported line-by-line): floating-by-default, **dynamic workspaces**,
overview-centric model, **overlay-key** (Super tap → overview), focus-stealing prevention
(user-time / xdg-activation timestamp rules), window placement (`place.c` semantics),
edge/half tiling, and the **~112 named keybinding actions** (workspace switch/move 1..12,
app/window/group/panel switchers, etc.). Encode the behaviors *you actually use* as a
~30-test conformance corpus (§8); let the rest drift.

### 3.4 The observable control plane (cross-cutting — first class)

A single, typed, **inspectable** `Command → Event → State` model for shell/WM **policy and
UI state** (windows, workspaces, focus, panel/quick-settings/overview state, settings).
Rust enums + exhaustive matching, pure-ish reducers, an observer/subscription layer that
the UI, a11y projection, D-Bus introspection, a **debug console** (a principled
replacement for Looking Glass / `Eval`), a **record/replay test harness**, and the
deferred extension host all consume.

This is a deliberate departure from GNOME Shell's worst structural trait — *"a web of
mutable global singletons and live monkeypatching, no clean module boundary to slice."*

**Caveat — split control plane from data plane:** policy/UI state is event-modeled,
recorded, and asserted. The per-frame **scene-graph/render path** (paint/pick at
60–240 Hz over thousands of actors) stays conventional retained-mode with damage
tracking, observable via **snapshots/taps**, not a serialized event log.

Payoffs beyond architecture cleanliness: behavior becomes a *checkable* claim (drive
synthetic events, assert state transitions) — which is how changes get verified without
screenshotting; the a11y tree, screencast metadata, and `org.gnome.Shell.Introspect`
become *projections* of one model; externalized UI state survives a shell-side crash.

### 3.5 Shell UI — per-surface placement (in-process vs layer-shell client)

**Litmus test:** a surface lives **in the compositor** iff it needs live zero-copy access
to *other clients'* pixels, or to transform/sample the live composited scene, or an
exclusive input grab, or security enforcement. Otherwise it can be a **layer-shell
client** (restartable → crash isolation).

| Surface | Placement | Why |
|---|---|---|
| Activities overview (window thumbnails, workspace strip, dash, app-grid *view*, search results presentation) | **In-compositor** | Live window rescale + one choreographed spring animation; cross-client animation is impossible |
| Alt-Tab / window cycling switchers | **In-compositor** | Live previews + input grab + instant raise |
| Magnifier / zoom + a11y color effects | **In-compositor** | Zoom-and-recolor the live output; no client mechanism exists |
| Screenshot/screencast interactive region UI | **In-compositor** | Live scene + grab |
| Lock / unlock / login UI | **In-compositor** (host `ext-session-lock` for 3rd-party lockers too) | Security; an out-of-process locker crash can wedge the session (niri #2986) |
| Top panel, quick settings/aggregate menu | **Decide (D2)** | In-compositor → free blur-behind + overview-integrated animation; client → crash isolation + off-the-shelf toolkit |
| Notifications, calendar/date menu, OSD, dialogs | **Client** (restartable) | Weak UX coupling, real failure risk |
| Wallpaper | In-compositor (overview zooms/blurs/dims it as one motion) | GNOME fidelity |

**One toolkit, two render targets:** the same widget tree paints either into a
compositor-scene render element *or* into a client `wl_surface`, so the in/out boundary is
a per-surface deployment flag and there's one CSS theme. (cosmic proves one toolkit can do
both via `IcedElement` in-process and standalone clients.)

**Internal contract:** a `compositor-core` crate exposes a typed **`ShellContext`** to the
`shell-ui` crate — enumerate/observe windows & workspaces, obtain live window textures as
render elements, push overlays into the scene, take/release input grabs, drive animations
on the frame clock. Client surfaces speak gated Wayland + D-Bus + GSettings.

**Crash hardening:** wrap in-compositor `shell-ui` update/paint in `catch_unwind` so a
widget panic degrades an overlay instead of aborting scanout.

### 3.6 `st-toolkit` — the St/Clutter replacement (the weakest link)

No production-ready CSS-themable *retained* Rust toolkit exists; Xilem/Masonry are alpha
(v0.1, 2026). **Assemble** a bespoke, standalone, headless-testable crate:

- **Layout:** Taffy (flex/grid).
- **Paint:** Vello (GPU 2D) for self-drawn UI; the effect set St needs — `BlurEffect`
  (dual-pass gaussian), invert-lightness, box-shadow, gradients, background-image — as
  shader passes.
- **Text:** Parley (see §3.7).
- **a11y:** AccessKit (node emission baked into the widget base class from widget #1 —
  cheap early, brutal to retrofit).
- **CSS:** `cssparser` + `selectors` to start (load the real `gnome-shell.css`); migrate
  to **Stylo** only if selector/cascade complexity demands it.

**Do not depend on the reactive layer (Xilem).** Pin/vendor Masonry/Parley/Vello at a
known rev; the alpha churn is a real tax (Experiment 3 quantifies it). **Strongly prefer
reusing libcosmic/iced for non-overview UI** until `st-toolkit` earns its place.

### 3.7 Text & IME — assemble, do **not** rewrite Pango

The two historical blockers are now **closed upstream in Rust**:

- **System fontconfig fallback:** `fontique 0.11` ships a *real libfontconfig backend*
  (`FcConfigSubstitute`/`FcFontMatch`/`FcFontSort` + `FcCharSet` script fallback) — honors
  the user's `/etc/fonts`, aliases, substitutions. (Earlier analyses that said "fontique
  doesn't read fontconfig" are now out of date.)
- **Editable/IME model:** `parley`'s editing module (`PlainEditor`/`Cursor`/`Selection`)
  already does bidi-aware visual caret movement, word/line movement, hit-testing,
  selection geometry, **IME preedit** (`set/clear/finish_compose`), and AccessKit
  integration — i.e. a Rust `ClutterText`.

So "rewrite Pango" reduces to: **assemble** `parley` + `fontique` + `harfrust` (HarfBuzz
v13 port) + `swash` (raster incl. COLR/sbix color emoji) + `icu_segmenter`/`icu_properties`
(line/word break, bidi) — all maintained crates — and write only **two pieces of glue**:
(a) a **glyph atlas + GPU upload** (swash → `etagere`/`guillotiere` → quads on the GLES
renderer; the direct analog of today's ~1.8k-LOC cogl-pango), and (b) the **St-widget ↔
parley binding + Wayland `text-input-v3`/IME wiring**.

**Keep `libfontconfig` (C)** via fontique — it's the one C dep worth keeping, because
byte-identical font matching to the rest of the desktop beats purity.

**Caveat (red-team):** IME is a known graveyard (`text-input-v3` ↔ `input-method-v2`
version mismatches silently break; COSMIC shipped with broken IME). **Wire Smithay's
text-input to real IBus/Fcitx5 and test CJK + dead-keys on day one** (Experiment 4). Keep
an FFI-to-Pango/HarfBuzz escape hatch behind a `TextEngine` trait for early bring-up and
any script that regresses.

### 3.8 D-Bus + GSettings — the drop-in contract surface

"Drop-in" = owning the right D-Bus names and GSettings schemas. The single compositor
process must **own both families** (today split between libmutter and the shell process):

- **Expose (own):** `org.gnome.Shell` + `org.gnome.Shell.Extensions` (`/org/gnome/Shell`),
  `org.gnome.Shell.Introspect`, `.Screenshot`, `org.gnome.ScreenSaver`, the fdo+gtk
  **Notifications** daemons, `org.freedesktop.impl.portal.Access`; and **all
  `org.gnome.Mutter.*`**: `DisplayConfig`, `IdleMonitor`, `RemoteDesktop`, `ScreenCast`,
  `InputCapture`, `ServiceChannel`, `Clipboard`, `ColorManager`, `X11`, `DebugControl`.
  These names are **hard-coded** by gnome-settings-daemon, gnome-control-center,
  gnome-remote-desktop, and xdg-desktop-portal-gnome — non-negotiable.
- **Consume:** logind, **gnome-session** (RegisterClient + EndSession handshake; expose
  `EndSessionDialog`), **GDM** greeter path (gdm session-mode), `gsd-*` (Color for Night
  Light, Power/Keyboard/Rfkill; and the **two-way** `GrabAccelerator` ↔ gsd-media-keys
  contract), **PipeWire** (screencast/remote-desktop transport), search providers, MPRIS,
  CalendarServer.
- **Own GSettings:** ship `org.gnome.shell.*` and `org.gnome.mutter.*` **verbatim**
  (ids/paths/keys, incl. the `org.gnome.shell.overrides` mirror). **Read-only** bind the
  external `org.gnome.desktop.*` (wm.preferences, wm.keybindings, interface, input-sources,
  a11y, peripherals) — those are owned by gsettings-desktop-schemas.

**Tooling:** `zbus` throughout; **codegen proxies/interfaces from the verbatim XML** in
`mutter/data/dbus-interfaces` and `gnome-shell/data/dbus-interfaces` to guarantee
signature fidelity. **`Eval` →** expose for signature compat but hard-refuse unless a dev
flag is set (never build a JS-eval bridge; it's a known hole, already gated behind
`unsafe_mode`).

**Reuse the existing GNOME daemons unchanged** (gsd, ibus, Orca, gnome-session, GDM,
xdg-desktop-portal-gnome, PipeWire/WirePlumber, NetworkManager, evolution-data-server) —
implement only the compositor-side contracts they talk to. Defer any daemon rewrite
indefinitely.

**Staging (D-Bus is large):** `DisplayConfig` + `IdleMonitor` + `ServiceChannel` first
(widest dependency; gets multi-monitor + power working) → `ScreenCast` (PipeWire producer,
fed from the capture/render path) → `RemoteDesktop` + `InputCapture` (libei/reis) →
`ColorManager`/Night Light → the rest. niri's `src/dbus/` is a direct porting reference.

`DisplayConfig` is the fiddly one: `ApplyMonitorsConfig(serial, method∈{verify,temporary,
persistent}, …)` with the `a((ssss)…)` monitor-tuple encoding; the confirm/revert countdown
lives in gnome-control-center, not the shell. Small encoding mismatches silently break the
Displays panel — XML-derived conformance tests from day one.

### 3.9 Accessibility

- **Keyboard:** cosmic-comp already serves `org.freedesktop.a11y.KeyboardMonitor` **and**
  `org.gnome.Orca.KeyboardMonitor` — Orca's keyboard path works out of the box; extend it
  (add the `PointerLocator` half).
- **Shell's own UI tree:** emit an **AccessKit** tree from `st-toolkit`, expose via
  `accesskit_unix` (AT-SPI2) — works today.
- **Client apps' tree (Newton):** GNOME's compositor-mediated push-model a11y relay is
  experimental and in flux (even GNOME's *own* shell UI still uses legacy AT-SPI under it).
  Implement the Newton relay **later**, pinned to mutter's protocol branch.

### 3.10 Rendering / HDR posture

**Decision (2026-07-04): own a Vulkan-native render stack now** — reversing this section's
earlier "defer the `ash`/Vulkan renderer to the Phase-4 endgame." The motivation is **not**
HDR but **zero-copy on virtio-gpu Venus + deleting the zink GL-translation layer**: the guest
runs GLES-over-zink-over-Venus today; a native Vulkan renderer talks to Venus directly. We
hand-roll bounded primitives on `ash` — our repo already owns the full finite shader set
(textured quad, SDF rounded-rect, dual-kawase blur, glyph atlas) — **not** wgpu/Skia/Vello
(wgpu hides dmabuf/explicit-sync/modifier control, the same reason §3.2 keeps GLES for
untrusted client buffers). The **Smithay GLES backend stays live behind a flag** through the
whole port (Smithay allows coexisting renderers); testability/verify-throughout is a
first-class constraint on this work.

**Staged (GLES daily-drives until the last step):**
- **(0) standalone ash-on-Venus offscreen spike — COMPLETE.** Device bring-up + unified quad
  pipeline (solid / SDF-rounded / textured) + dual-kawase blur + hinted cosmic-text/swash
  glyph-atlas text vs a pango reference, all verified on both Venus and lavapipe via structural
  pixel-invariant `cargo test` (no golden images), plus forward-looking DRM-modifier /
  external-semaphore probes. Lives in `niri-vk/` (workspace member, promoted from the spike into a
  reusable ash primitive **library** + a headless bring-up/CI binary, so the Stage-2 Vulkan
  renderer consumes the same low-level pieces; its ash/png deps reach the niri binary only behind
  the opt-in `vulkan` feature). **Text at 1× must stay crisp → hinted glyph atlas, not
  GPU-raster-into-atlas.**
- **(1) dmabuf import with DRM modifiers on Venus** — the #1 front-loaded risk. Probes show
  Venus exposes only the LINEAR modifier here, so scope the importer to linear.
- **(2) Vulkan `RenderElement` + Smithay renderer-trait twins behind `--renderer=vulkan`** —
  starts as a `NiriRenderer` **trait redesign** (renderer-enum + niri-owned primitive API),
  not a second impl bolted beside the Gles hardwiring.
- **(3) KMS scanout + explicit sync** — see the measured Venus explicit-sync constraints in
  `docs/fork/venus-explicit-sync-gap.md` (bridge via kernel `drm_syncobj` timeline ⟷
  `sync_file` ⟷ binary `SYNC_FD` VkSemaphore; `OPAQUE_FD`/Vulkan-timeline export is absent and
  structurally can't cross virtio).
- **(4) cut over at parity; delete GLES/pango/cairo.**

**Known gaps of the owned renderer vs GLES — see `docs/fork/renderer-gaps.md`.** Deleting GLES is
**not** a one-way door: single-device / LINEAR-only / single-plane are configurations of the Vulkan
renderer, not its architecture, and Smithay's multi-GPU machinery is GLES-typed top to bottom, so
keeping it would buy no head start on a Vulkan implementation. The gap that bites **first, and on
every machine including the VM**, is not multi-GPU but **multi-planar dmabuf import (NV12/P010 →
zero-copy hardware video decode)**; multi-GPU is bounded, mostly-mechanical work whose real cost is
driver validation on bare metal we don't have.

**HDR/wide-gamut/color-management stays deferred** — an industry-wide moving target
(cosmic-comp's own Vulkan/HDR work is Epoch 2–3, 2026–27), and Vulkan alone ≠ HDR (color-mgmt
is still WIP upstream). **Night Light is cheap and mostly reusable**: consume `gsd-color`'s
`Temperature` → apply per-CRTC gamma LUTs; the schedule stays in gsd-color.

---

## 4. Extensions — deferred, tiered, non-blocking

Settled technical point: today's extensions bind **two** surfaces — (1) `imports.gi.*`
GObject typelibs (St/Clutter/Meta/Shell **plus** the whole platform: Gio/GLib/Pango/NM/…),
and (2) the `js/ui` JS module graph they monkeypatch. `imports.gi.St` is an **interface**,
not an implementation — GObject-Introspection is merely *GJS's* way of satisfying it. So
**extension support does not require GObject in the core.** But:

- **Unmodified existing extensions** require GJS + the live GObject type system + ABI-compatible
  St/Clutter/Meta/Shell + the identical `js/ui` graph — i.e. exactly the four things the
  endgame sheds. We will **not** provide that. Existing extensions are a *port* story, not
  drop-in.
- A **redefined JS API** (your framing) is the right fork choice: an **outboard, optional,
  versioned shim** on top of a GObject-free core — embed **rquickjs** (QuickJS-ng: tiny,
  fast startup, per-extension Runtime for memory/CPU caps; keep the binding layer
  engine-agnostic so deno_core/V8 is swappable if TS/DevTools demand it), expose a
  **curated, capability-scoped API** via hand-written bindings (no introspection):
  `addIndicator`, `addQuickToggle`, `registerSearchProvider`, `addKeybinding`, `notify`,
  `settings`, plus enumerated **overview/workspace/app-grid/placement policy hooks** instead
  of arbitrary private-method monkeypatching.

**Tier ladder (stop at any rung; each is additive, none reaches into the core):**
- T0 none → T1 curated native API for the common patterns → T2 faithful `imports.gi.{St,
  Clutter,Meta,Shell}` reproduction on the embedded engine → T3 full platform `gi` surface
  + `js/ui` shapes ("full GJS-equivalent").

**Inventory-driven:** extensions.gnome.org exposes install counts; rank by installs,
measure the `gi`/`js/ui` surface the top N% actually touch (a small API covers most
*installs*). Keep `org.gnome.Shell.SearchProvider2` (already a GObject-free D-Bus
contract) verbatim so the **entire remote-search ecosystem works with zero engine work**;
keep `org.gnome.Shell.Extensions` + `OpenExtensionPrefs` so Extension Manager / CLI keep
functioning.

**Red-team caveat:** "extension compatible" will be read as "my .zip runs unmodified" — it
won't. Say so loudly. The compat shim has a hard ceiling (it can fake leaf-widget
composition, not real GType identity / `instanceof Clutter.Actor` / vfunc overrides). And
**your own `js/ui` logic** is the deeper question: if you want it in JS you've kept an
engine; if not, write shell logic in Rust and expose no scripting initially.

---

## 5. Multi-project decomposition (repos / crates)

A clean dependency DAG, each piece independently testable; bottom depends on nothing above.

```
text-stack         parley+fontique+harfrust+swash+icu glue + glyph atlas + TextEngine trait
   ▲
st-toolkit         retained CSS scene-graph: Taffy + Vello + text-stack + AccessKit +
   ▲               cssparser/selectors; effects (blur/invert/shadow); 2 render targets
   │
gnome-contracts    zbus bindings (org.gnome.Shell.*/Mutter.*/SettingsDaemon.*/a11y) from XML;
   ▲               typed GSettings access; wayland-protocol-extensions crate
   │
compositor-core    fork of niri (or cosmic-comp): Smithay backend + protocols + XWayland;
   ▲               GNOME WM policy; the control-plane state/event model; ShellContext API
   │
shell-ui           in-process module: overview, panel, quick-settings, app-grid, search,
   ▲               alt-tab, OSD, lock, magnifier — on st-toolkit + ShellContext
   │
shell-clients      layer-shell clients: notifications, calendar, OSD, dialogs (libcosmic/iced
                   short-term, st-toolkit later)
extension-host     (deferred) rquickjs + capability API + js/ui-shim (depends on shell-ui)
```

Already-separate helper services (calendar-server, screencast recorder, screenshot,
search) stay as small out-of-process binaries for fault isolation.

---

## 6. Roadmap — sequenced to *always usable*

### Phase −1: De-risking experiments FIRST (~6–8 weeks total)

Do these **before committing years**. Any one can redirect the whole plan.

1. **(decisive, 2–4 wk) Live-overview spike.** Can a GNOME-faithful overview hit 60 fps
   with live window thumbnails? Try it niri-style (in-compositor rescale). Also try the
   out-of-process capture version honestly — if *that's* acceptable, you sidestep the whole
   in-process-toolkit long pole and can use libcosmic/iced.
2. **(1–2 wk) Base selection ("de-COSMIC" / "de-niri").** Fork each candidate, strip its
   policy + ecosystem config, boot from GSettings + a minimal GNOME keybinding set, measure
   what % of its `src/shell` survives. **Pick by how much you delete** (§2.2).
3. **(1 wk) Toolkit churn probe.** Pin Masonry/Parley/Vello; build *one* St-equivalent
   widget set (ScrollView+Button+Label) rendering the real `gnome-shell.css`; re-pin 6
   months later and count breakages. Quantifies the alpha tax.
4. **(1 wk) IME/text reality check.** Wire Smithay `text-input-v3`/`input-method-v2` to real
   IBus + Fcitx5, type CJK + dead-keys; write the minimal fontconfig→fallback path, verify
   CJK/emoji.
5. **(1 wk) Conformance-corpus rot test.** Write ~30 behavior tests (D-Bus signatures +
   GSettings + key Wayland behaviors + a few overview gestures); run vs GNOME 50, then vs
   GNOME 51 when it lands. Validates "conformance corpus, not source-as-spec."

### Phase 0: Daily-drive from week one
Run the chosen base **as-is** as your session. Add the **build/CI + conformance harness**.
Change **one** GNOME-flavored thing you care about (e.g. overlay-key → a stub overview, or
dynamic-workspace behavior). Ship it to yourself. This is the charter in action.

### Phase 1: GNOME identity & contracts (always usable)
Own the D-Bus names (`DisplayConfig` + `IdleMonitor` + `ServiceChannel` + `org.gnome.Shell`
minimal) so gnome-session/gsd/control-center accept you; GSettings-backed config; GNOME WM
policy (focus/placement/dynamic workspaces/overlay-key/~112 keybindings); gnome-session +
EndSessionDialog; Night Light. The control-plane state/event model lands here.

### Phase 2: The overview & core shell UI (gated on Experiment 1)
In-compositor overview (live thumbnails, workspace strip, dash, app-grid view, search via
`SearchProvider2`), alt-tab switchers, OSDs, lock screen. Toolkit per Experiment 3:
`st-toolkit` if it earns it, else libcosmic/iced.

### Phase 3: Platform completeness
ScreenCast (PipeWire producer) → RemoteDesktop/InputCapture → Screenshot/portal backend →
notifications/calendar clients → OSK → IBus candidate UI → AccessKit a11y → magnifier
parity.

### Phase 4+: The endgame (optional, last)
Shed GObject/Cogl/Clutter where it still lingers; **HDR/color-management** on the owned Vulkan
renderer (the renderer itself is being built now per §3.10, ahead of this phase); the
extension host (§4); a fully native `st-toolkit` everywhere. None of this gates usefulness.

---

## 7. Risk register

| Risk | Sev | Mitigation |
|---|---|---|
| **Bus factor:** 3–4 funded-team domains on one person | **fatal** | Reuse much, own little; drop extensions, bug-for-bug, custom text stack early; the only way solo survives |
| **Unusable valley / motivation** over a decade | **fatal** | Daily-drive from week one; forbid any milestone not usable the day it lands; charter = "personal GNOME-flavored desktop," not "drop-in replacement" |
| **In-process overview is the irreducible long pole** | high→ resolved-in-principle | niri proves in-compositor live overview in Rust; gate on Experiment 1; out-of-process capture is the fallback |
| **Rebuilding St/Clutter on alpha foundations (Xilem/Masonry/Parley/Vello)** | high | Avoid the reactive layer; pin/vendor; reuse libcosmic/iced until st-toolkit earns its place; Experiment 3 |
| **cosmic-comp's policy fights GNOME's** | high | Prefer niri base; pick base by deletion; Experiment 2 |
| **You can't actually shed GObject early** (15 platform `gi` integrations: Gio/GLib/NM/UPower/Polkit/Gvc/…) | high | Keep `gio-rs`/`glib-rs` for plumbing indefinitely; "no GObject anywhere" is aspirational, scheduled last |
| **Chasing fast-moving GNOME upstream** (ships every 6 mo) | high | Freeze a target ("GNOME 50 behaviors"); re-baseline every 2–4 cycles, not every cycle; ~30 conformance tests, let the rest drift |
| **Text + IME rabbit hole** | high | Assemble, don't rewrite; wire real IBus/Fcitx5 day one (Experiment 4); FFI-to-Pango escape hatch |
| **Accessibility skipped → excludes Orca users, hard to retrofit** | medium | Bake AccessKit into the widget base class from widget #1, or state explicitly the fork isn't accessible |
| **Frame-pacing/damage/latency parity** (mutter took a decade; triple-buffering took 5 yrs) | medium | Lean on Smithay's renderer + damage; don't reinvent the frame loop; set a latency bar for the overview spike |
| **In-compositor shell panic kills the session** | medium | `catch_unwind` around shell-ui update/paint; route failure-prone surfaces to restartable clients |
| **Privileged-protocol attack surface** (screencopy/toplevel/workspace control) | medium | Reuse cosmic-comp's `client_not_sandboxed()` + `wp_security_context_v1` gating verbatim |
| **DisplayConfig signature drift breaks Displays panel** | medium | XML-derived conformance tests from day one |
| **Redefined JS API quietly re-imposes GObject** | medium | Decide early if `js/ui` logic is JS at all; if not, Rust shell logic + no scripting initially |

---

## 8. Conformance corpus (replaces "bug-for-bug")

Real bug-for-bug fidelity only matters at **external wire contracts** (Wayland
serial-validity windows, frame-callback throttling apps gate their loops on, XWayland
`_NET_WM_SYNC_REQUEST`/INCR-clipboard quirks). Everywhere else the spec is a **machine-
checkable corpus**, not the old source:

- **D-Bus:** signatures generated from the verbatim XML; CI signature-diff per release.
- **GSettings:** a key-support matrix (no key renamed/retyped without a migration).
- **Wayland/XWayland:** a real-app behavior battery (GTK/Qt/Electron/SDL popups, DnD,
  drag-resize, focus, clipboard).
- **WM behavior:** ~30 encoded behaviors *you actually use* (overview gestures, focus-steal,
  placement, tiling, keybindings).
- **Text:** golden line-break/bidi/caret/shaping vs current Pango for Latin/Arabic/
  Devanagari/CJK/emoji.
- **Do-not-port list:** deliberate breaks (e.g. `Eval` stays a refusing no-op).

The control-plane event model (§3.4) makes most of this *record/replay* rather than manual.

---

## 9. Licensing

cosmic-comp is **GPL-3.0-only**; mutter/gnome-shell are **GPL-2.0-or-later** (upgrades
cleanly to GPL-3.0). So importing GNOME code into a cosmic-comp-derived fork is **legal**;
the combined work is **GPL-3.0-only** and **not re-mergeable upstream into mutter** —
acceptable since upstreaming isn't a goal. libcosmic (MPL-2.0), iced (MIT), libcroco
(LGPL) are all GPL-3.0-compatible. niri is GPL-3.0. Document this clearly for contributors.

---

## 10. Open decisions for you

- **D1 — Base:** ✅ **RESOLVED (2026-07) — niri, as a hard fork** (no upstream rebasing).
- **D2 — Panel/quick-settings placement:** ✅ **RESOLVED (2026-07) — in-compositor.** Panel
  landed (opaque bar + Activities + live clock + top strut); quick-settings still to build.
- **D3 — Drop-in bar:** full D-Bus/contract drop-in (gnome-session/GDM/gsd/portal accept us)
  vs UX/behavior-level only first. Recommend staged contracts (§3.8).
- **D4 — Toolkit:** assemble `st-toolkit` (max control, matches GNOME CSS, most work) vs
  adopt libcosmic/iced (faster, proven dual in/out, harder to make pixel-faithful to Adwaita).
  Gate on Experiment 3.
- **D5 — `js/ui` language:** Rust shell logic (drops GJS, no early extensions) vs keep a JS
  engine for shell logic (keeps an engine but eases a future extension tier).
- **D6 — Scope of "always usable":** confirm the reframed charter — daily-drive from week
  one — vs the maximalist "drop-in" framing (which the red-team rates as not solo-sane).

---

## Appendix: grounding facts

- mutter 50.1: ~610k LOC C (cogl ~69k shrinking, clutter ~112k, mtk ~3.5k, src ~357k). 0 Rust.
- gnome-shell 50.1: ~83k LOC C (St ~57k incl. ~17–20k vendored libcroco) + ~81k LOC GJS
  (164 files, 365 `GObject.registerClass`). Extension `gi` reach: Clutter 93 files, St 88,
  Shell 69, Meta 45.
- cosmic-comp v1.0: ~65k LOC Rust; backend ~9.9k (KEEP), `src/shell` ~32k (COSMIC policy,
  REPLACE), GPL-3.0-only, RON config, near-empty D-Bus.
- niri: single-process, in-compositor live overview (`RescaleRenderElement`), implements
  GNOME/Mutter D-Bus, pango/pangocairo UI text. GPL-3.0.
- Key crates: smithay, drm-rs/gbm/input/libseat/calloop, zbus, parley/fontique 0.11/
  harfrust 0.10/swash 0.2/skrifa/icu_* 2.x, Taffy, Vello, AccessKit, cssparser/selectors,
  rquickjs. Reference compositors: niri, cosmic-comp, anvil.
</content>
</invoke>
