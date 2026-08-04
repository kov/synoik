# Port-level audio model — scope + plan

Status: **slices 0–3 landed 2026-07-31** (`906cb2de`, `04a513a2`, `8e0a635f`, `3a899729`);
card-profile switching is in the backlog (§8). This was item 3 of what was left of slice F in
`docs/fork/osd-media-port.md`. Each slice below carries what actually landed, and where the plan was
wrong.

References read for this plan (gnome-shell 50.3 checkout, `~/Projects/gnome-shell`):
`js/ui/status/volume.js`, `subprojects/gvc/gvc-mixer-control.c`,
`subprojects/gvc/gvc-mixer-ui-device.{c,h}`.

## 1. Where we are

Our audio model is **sink/source-level**:

- `AudioStatus { volume, muted }` (`src/audio.rs:25-30`), `MicStatus` (`src/audio.rs:46-63`).
- `SinkInfo { name, description }` / `SourceInfo` (`src/audio.rs:93-125`), where `name` is the
  PipeWire `node.name` and doubles as the key we write to `default.configured.audio.sink`.
- `src/pipewire_audio.rs` tracks `Audio/Sink` and `Audio/Source*` **nodes** and the `default`
  metadata. It contains no concept of a port: grep for "port" → 0 hits. It never binds an
  `ObjectType::Device`.
- The QS pickers render one row per node, label = `description`, no icon
  (`src/ui/quick_settings.rs:631-653` and `:669-694`, `icons: Vec::new()`).

## 2. What GNOME actually does

GNOME's list is **port-level**, built by gvc, not stream-level:

- `create_ui_device_from_port` (`gvc-mixer-control.c:1973-2006`) makes one `GvcMixerUIDevice` per
  **card port**, with `description` = the port's human description ("Headphones"), `origin` = the
  card name ("Built-in Audio"), `port-available` = `port->available != PA_PORT_AVAILABLE_NO`.
- Ports that are unavailable are not offered: `output-added` / `output-removed` are emitted purely
  off the availability flip (`gvc-mixer-control.c:2019-2047`), so unplugging headphones *removes*
  the row rather than leaving a dead one.
- Streams with no ports still get a device, via a fallback in `sync_devices`
  (`gvc-mixer-control.c:1354-1377`): `description` = the stream description, `origin` = `""`,
  `port-available` = TRUE. This is what keeps a null sink or a portless bluetooth sink listed.
- The shell renders `description – origin` when `origin` is non-empty, else `description`, as a
  `PopupImageMenuItem` carrying `device.get_gicon()` (`volume.js:126-137`). The icon is the
  device's `icon_name`, falling back to the **card's** icon (`gvc-mixer-ui-device.c:632-643`).
- The menu is only enabled with **more than one** device (`volume.js:171-175`,
  `menuEnabled = this._deviceItems.size > 1`) — which we already mirror per-list
  (`src/ui/quick_settings.rs:1486-1489`).

And the headphone behaviour hangs off the *active* port of the current sink:

```js
_findHeadphones(sink) {                                    // volume.js:332-345
    if (sink.get_form_factor() === 'headset' ||
        sink.get_form_factor() === 'headphone')
        return true;
    if (sink.get_ports().length > 0)
        return sink.get_port().port.toLowerCase().includes('headphone');
    return false;
}

_portChanged() {                                           // volume.js:347-358
    const hasHeadphones = this._findHeadphones(this._stream);
    if (hasHeadphones === this._hasHeadphones)
        return;
    const initializing = this._hasHeadphones === undefined;
    this._hasHeadphones = hasHeadphones;
    this._updateIcon();
    if (!initializing)
        this.showOSD();
}
```

Three details worth pinning, because all three are easy to get wrong:

1. **The OSD is suppressed on the first sync only** — `initializing` is `_hasHeadphones ===
   undefined`, i.e. exactly once per shell lifetime, *not* once per stream.
2. **`_hasHeadphones` is not reset when the default sink changes.** `_connectStream` calls
   `_portChanged()` (`volume.js:315-322`), so switching the default from a headphone sink to a
   speaker sink is a change and **does** show the OSD.
3. **The headphone glyph beats the level glyph, including muted** — `_updateIcon` is
   `hasHeadphones ? 'audio-headphones-symbolic' : this.getIcon()` (`volume.js:359-363`), and
   `getIcon()` is the only thing that consults mute.

## 3. Divergences this closes

| # | Divergence | Recorded at |
|---|---|---|
| D1 | No headphone-plug OSD, and the indicator never becomes `audio-headphones-symbolic` | new |
| D2 | Device lists are node-level, not port-level — a multi-port card shows one row where GNOME shows "Speakers" and "Headphones" | `src/audio.rs:92-93`, `docs/fork/panel-status-port.md` Q3 |
| D3 | Device rows carry no icon and no ` – origin` suffix | new, found writing this plan |

## 4. The PipeWire mapping

gvc's card/port pair maps onto PipeWire's **Device** object and its route params. Everything needed
exists in the crates we already have (`pipewire` 0.9.2):

- `pipewire::device::Device` is a bindable proxy with `subscribe_params` / a `param` listener /
  `set_param` (`device.rs:17-81`), the same shape `bind_default` already uses for nodes.
- `ParamType::EnumRoute` / `ParamType::Route` exist (`libspa-0.9.2/src/param/mod.rs:44-47`).
- The route object fields are all in the generated bindings: `SPA_PARAM_ROUTE_{index, direction,
  device, name, description, priority, available, profiles, props, devices, profile, save}`, and
  `SPA_PARAM_AVAILABILITY_{unknown, no, yes}`.

So: `EnumRoute` enumerates every port on the card (gvc's port list), `Route` reports the **active**
route per direction, and writing `Route` selects one (what `wpctl set-route` / pavucontrol do).

**Join key — confirmed on live hardware** (see §6 for how to look). To attribute an active route to
the default *sink node* we join on the node props `device.id` (the Device global id,
`PW_KEY_DEVICE_ID`, `/usr/include/pipewire-0.3/pipewire/keys.h:271`) plus `card.profile.device` (the
SPA device index inside the card), matched against `SPA_PARAM_ROUTE_device`. `card.profile.device`
is a WirePlumber/pipewire-pulse convention rather than a documented spa key, but this machine's sink
node carries both, and the value matches the active route's `device`:

```
DEVICE 42  Audio/Device  alsa_card.platform-a016000.virtio_mmio  "Built-in Audio"  icon=audio-card-analog
 NODE 49   Audio/Sink    alsa_output...stereo-fallback  device.id=42  card.profile.device=1
 Route     index=0 direction=Output name="analog-output" description="Analog Output"
           available=unknown  device=1  profile=1  devices=[1]  props={mute, channelVolumes, …}
```

Two parser details visible there: an `EnumRoute` entry carries `devices: [1]` (an array of the SPA
device indices the route applies to) while the active `Route` carries **both** `device: 1` (scalar,
the one it is applied to) and `devices: [1]`. And `available` is `"unknown"` here, not `"yes"` —
which is exactly why the filter must be gvc's `!= no` rather than `== yes`, or this card's only
output would vanish from the list.

`device.form_factor` (gvc's `sink.get_form_factor()`) rides on the node props and can be captured in
the node's **`info` event** (see slice 1 — *not* the registry global, which was this port's most
expensive mistake). This card does not set it.

## 5. Slices

### Slice 0 — the audio seam — **LANDED** (`906cb2de`)

`Synoik::pw_audio` is a concrete `Option<PwAudio>` behind `#[cfg(feature = "pipewire")]`
(`src/synoik.rs:549-551`), so a headless fixture has no audio at all and **nothing** about the audio
wiring is testable. The gap is already recorded and real: deleting the `show_volume_osd` call inside
`adjust_volume_by_scroll` leaves the whole suite green.

- Introduce `trait AudioBackend` in `src/audio.rs` (unconditional, no PipeWire types in the
  signature) covering the surface the compositor actually calls today: `status`, `mic_status`,
  `set_volume`, `adjust_volume`, `set_muted`, `toggle_muted`, `set_input_volume`, `set_input_muted`,
  `toggle_input_muted`, `set_default_sink`, `set_default_source`.
- `PwAudio` implements it; `Synoik::audio_backend: Option<Box<dyn AudioBackend>>` replaces the cfg'd
  field, which deletes the six `#[cfg(feature = "pipewire")]` arms and the
  `#[cfg(not(feature = "pipewire"))]` catch-all in `src/input/mod.rs:1204-1266`.
- Add a `StubAudio` for the fixture: holds an `AudioStatus`/`MicStatus`, records writes, and lets a
  test *drive* the real entry points (`State::on_audio_status`, `on_sink_list`, …) — per
  "test the code, not a reimplementation", the seam goes at the real entry point.
- **Pins immediately, before any new behaviour:** panel scroll → volume step → OSD; QS slider →
  `set_volume`; picker row → `set_default_sink`; the mic slider's visibility rule.

Small and mechanical, and it is the difference between slices 2–3 landing tested or landing blind.

**As landed**, all of the above plus:

- `Fixture::install_stub_audio(volume)` seeds the backend *through* `State::on_audio_status` rather
  than assigning `niri.audio` — that call is also what puts the volume icon in the panel cluster, so
  a test that sets the field directly has nothing to aim a scroll at.
- Two new tests: `a_scroll_over_the_volume_icon_steps_the_backend_and_shows_the_osd` (wheel notch →
  one `SCROLL_STEP` write → OSD; the ceiling case writes but shows nothing; with QS open it writes
  nothing and shows the OSD) and `the_quick_settings_audio_controls_reach_the_backend` (sliders,
  mute toggles, both pickers, and the no-source-bound path).
- Both are **mutation-checked**, not just green: deleting the `show_volume_osd` call fails "the
  scroll shows an OSD", and dropping the did-it-move gate fails "scrolling up at the ceiling must
  not keep re-arming the OSD". Worth repeating for slices 2–3 — the recorded gap existed precisely
  because a green suite proved nothing here.
- Note the trait made `--no-default-features --features dbus,systemd` *better*, not worse: the audio
  call sites no longer need a cfg at all. (That build still fails on two pre-existing
  screen-recording errors unrelated to audio.)

### Slice 1 — Device + route watcher (read-only) — **LANDED** (`04a513a2`)

- Track `ObjectType::Device` globals whose `media.class` starts with `Audio/`; capture
  `device.description`, `device.icon-name`, and the id, the same way sinks are captured today.
- Bind each and `subscribe_params(&[ParamType::EnumRoute, ParamType::Route])`.
- Parse routes into a plain model in `src/audio.rs` (no PipeWire types), e.g.
  `CardPort { card_id, index, device, direction, name, description, priority, available }` plus the
  active `(direction, device) -> index` map per card.
- Capture `device.form_factor` on sink nodes in `on_global`.
- Publish on change with the same dirty/last dedup shape as `publish_sinks`.
- Tests: the pod→model parse is a pure function over a serialized `Route`/`EnumRoute` object, so it
  unit-tests without PipeWire (build the pod with `PodSerializer`, same trick as
  `sink_default_json_round_trips_through_the_parser`).

**A registry global's props are a SUBSET — the trap this slice hit.** The first live run produced a
perfectly well-formed model with `icon_name: None`, `card: {card_id: 42, device: None}` and no form
factor, i.e. permanently empty in exactly the fields slice 2 needs. `pw-dump` shows those fields, so
the model looked wrong against reality but right against the code. The cause: PipeWire's *registry
global* callback hands you a small dict — for this card, `device.description/nick/name`,
`media.class`, `object.path` — and **not** `device.icon-name`; for the sink node, `device.id` but
**not** `card.profile.device` and not `device.form_factor`. Those live only in the `info` event of a
*bound* proxy. Fix: `.info()` listeners on the bound sink node and on the card, each refusing to
overwrite a known value with `None` (info repeats with partial change masks).

That is also why [`BoundSinkInfo`] hangs off the bound sink rather than off every `SinkInfo`: we
bind exactly one sink (the default), so it is the only one that can honestly report these — and it
is the only one GNOME's `_findHeadphones` asks about anyway.

**Generalise it:** no headless test could have caught this. The model was structurally correct and
silently empty. Any field sourced from a registry global needs one live read to confirm it is
actually there.

**As landed**, validated on the real card end to end (instrumented build, run as gsrs against the
seat's PipeWire, then reverted): `icon_name: Some("audio-card-analog")`, bound sink
`card: {card_id: 42, device: Some(1)}`, active route `analog-output` with `device: Some(1)` — the
join resolves. `form_factor: None` is correct here; this card does not set one.

### Slice 2 — headphones: icon + OSD — **LANDED** (`8e0a635f`), with a correction

- `audio::has_headphones(form_factor: Option<&str>, active_port: Option<&str>) -> bool` — a direct,
  pure port of `_findHeadphones`, with its "no ports at all → false" branch.
- ~~`AudioStatus` gains `headphones: bool`, so `volume_icon` stays a pure function of the status and
  both the panel and the QS slider pick it up for free.~~ **WRONG — corrected while implementing.**
  That would have put the headphone glyph on the panel indicator and the OSD, and GNOME puts it on
  neither. `_updateIcon` sets `this.iconName`, the **quick-settings slider's own button**; the panel
  indicator is assigned separately from `this._output.getIcon()` in the `stream-updated` handler
  (`volume.js:484-490`), and `showOSD` builds its gicon from `this.getIcon()` (`volume.js:283-288`).
  Both of those are the plain level icon. So the override is one function,
  `audio::output_slider_icon`, called from `quick_settings.rs` **only** — `volume_icon` is untouched
  and keeps feeding the panel and the OSD. (Reference-first caught this; the plan was written from
  a reading of `_findHeadphones`/`_portChanged` without following where `iconName` actually lands.)
- Port-change OSD with the initial-sync suppression: the watcher keeps `Option<bool>`, and emits an
  OSD request only when the previous value is `Some(_)` and differs — never reset across a
  default-sink change (detail 2). Return it as data the way `BrightnessUpdate` carries `OsdRequest`
  (`src/brightness.rs`), rather than reaching into the OSD manager from the algebra; the compositor
  side reuses `show_volume_osd` / `osd.show_all`.
- Tests (now possible because of slice 0): plug headphones → icon flips and an OSD appears; the
  *first* sync flips the icon and shows **no** OSD; muted + headphones still shows the headphone
  glyph; default-sink swap from a headphone sink to a speaker sink shows an OSD.

**As landed:** `has_headphones` is a branch-for-branch port (note the middle branch *returns* — once
a sink has ports the answer is the port name and nothing else), `default_sink_has_headphones`
resolves it against the models and returns `Option<bool>` where `None` is "no sink bound, no
answer". That distinction is load-bearing: an unbound period must not spend the one-time
suppression. `Synoik::headphones: Option<bool>` is `_hasHeadphones`, and is deliberately **not** reset
on a sink swap.

Both traps are mutation-checked: dropping the `!initializing` guard fails *"the initial sync must
not raise an OSD"*, and resetting the answer per stream fails *"`_hasHeadphones` is not reset per
stream"*.

**Still needs hardware.** Every test here drives the models directly; nothing has confirmed that a
real jack produces the route change we expect. On this card the answer is `Some(false)` forever.

### Slice 3 — port-level device lists — **LANDED** (`3a899729`)

- Replace `SinkInfo`/`SourceInfo` with a shared `AudioDevice { key, description, origin, icon,
  available }`, keyed by `(card global id, direction, route index)` for port-backed devices and by
  `node.name` for the portless fallback.
- Build the list from ports, filtering `available == SPA_PARAM_AVAILABILITY_no` out
  (`gvc-mixer-control.c:1973,1995`), and add the portless-stream fallback (`:1354-1377`) so a null
  sink or a portless bluetooth sink stays listed.
- Row label `description – origin` (en dash, `volume.js:130-133`); row icon from the device icon
  falling back to the card icon (`gvc-mixer-ui-device.c:632-643`) into the existing `ItemRow.icons`.
  Closes D3.
- Activation writes the `Route` param to select the port, **then** sets the default node. The order
  matters: making a node default before selecting its port lands you on the old port. Same as gvc's
  `change_output` case 3.

**Card-profile switching: deferred, kept in the backlog** (agreed 2026-07-31). gvc reaches a port in
an inactive profile by swapping the card's profile first (`change_profile_on_selected_device`,
`gvc-mixer-control.c:1590-1600`). We do not, for two reasons: it is *stateful async sequencing* —
set profile, wait for nodes to be republished, then set route, then set default, which is what gvc
carries `profile_swapping_device_id` across — and there is no cross-profile route on this machine to
test it with (three profiles, but only `output:stereo-fallback` carries a route).

Deferring the switch forces a choice about the *list*, because `EnumRoute` returns routes from
inactive profiles too. Three options were on the table: implement switching now; list only
in-profile routes; or list everything and let those rows silently fail. **We took the middle one** —
`AudioCard::offerable_ports` filters on the active profile, so every row that exists works.

**Known divergence, recorded deliberately:** GNOME lists cross-profile ports and switches for you;
we omit them. In practice that costs the HDMI row on multi-profile cards, and bluetooth A2DP↔HSP/HFP.
Pinned by `unavailable_and_out_of_profile_ports_are_not_offered`.

## 6. Reading the real card — you must dump as the seat user

There **is** a real card here (`/proc/asound/cards`: `virtio-snd VirtIO SoundCard`), but a plain
`pw-dump` as `kov` shows only `auto_null` and no `Audio/Device` at all. That is not the absence of
hardware, it is logind device ACLs: `/dev/snd/*` is granted to the **active seat session** only,
which is `gsrs`.

```
$ getfacl -p /dev/snd/controlC0
user::rw-
user:gsrs:rw-        <- the seat session; kov is not on the list
group::rw-
```

So kov's own PipeWire daemon runs an ALSA monitor that finds nothing, and every audio question asked
from a kov shell answers "no hardware". Dump from the seat session instead (read-only, safe):

```
sudo -u gsrs XDG_RUNTIME_DIR=/run/user/1002 pw-dump
```

**Trap for this whole port:** any "does PipeWire expose X" check must be run that way, or it will
come back a confident, wrong "no". The same applies to a compositor built to test this — a headless
niri started as kov will never see a route.

What this card can and cannot validate:

- **Can:** slice 1's whole read path — a real `Audio/Device`, real `EnumRoute`/`Route` objects, the
  node→device join, the `available: unknown` case, and per-route `props`.
- **Cannot:** the headphone behaviour. This card exposes exactly one output route, "Analog Output",
  with no headphone port and no `device.form_factor`, so there is nothing to plug in and
  `has_headphones` is `false` by every branch. Slice 2's OSD and icon flip still need real hardware
  with a jack (or a bluetooth headset, which gvc's `form_factor` branch was written for) — it goes
  on the pending-live-validation list.
- **Cannot:** the multi-row picker. One route means slice 3 renders one row, same as today.

Unit tests therefore still build **hand-made pods** for the multi-port, availability-flip and
headphone cases; the live card is the check that the parser agrees with reality on at least one
real device.

## 7. Open questions to settle before slice 2

1. ~~Confirm `card.profile.device`~~ — **resolved**, see §4: `device.id=42` + `card.profile.device=1`
   on the sink node, matching `Route.device=1`. Keep the fallback anyway (join on `device.id` alone
   and take the single active output route) for cards that omit it.
2. Do we want per-port volume? PulseAudio stores volume per port, and PipeWire does expose it —
   this card's active `Route.props` carries `mute` + `channelVolumes` + `volumeStep`. Proposal:
   still **no** — the node `Props` stay the single volume authority; the node echo will report
   whatever the port switch did to the volume.
3. `MAX_DEVICE_ROWS` (`src/ui/quick_settings.rs`) becomes more likely to bite once rows are
   per-port rather than per-card. Worth re-checking the cap against GNOME (which has none).

## 8. Backlog

- **Card-profile switching** (low priority, needs hardware). See slice 3 above for why it is out and
  what it costs. Would restore the HDMI and bluetooth-mode rows. Wants `EnumProfile` parsed (we
  already read the active `Profile` index), a `set_profile` on the backend, and the async
  re-enumeration sequenced properly.
- **`MAX_DEVICE_ROWS`** now truncates a *port* list rather than a card list, so it bites sooner.
  GNOME has no cap. Worth revisiting.
- **Per-port volume.** PulseAudio stores volume per port and PipeWire exposes it in
  `SPA_PARAM_ROUTE_props`; we keep the node `Props` as the single volume authority (§7 Q2).

## 9. What is live-validated, and what is not

Validated on this VM's virtio-snd card (instrumented build run as gsrs against the seat's PipeWire,
then reverted — see §6 for how):

- slice 1's whole read path: card, `EnumRoute`, `Route`, `Profile`, the node→card join, the icon;
- slice 3's list: one row, `Analog Output – Built-in Audio`, `audio-card-analog`, `selected: true`,
  key resolving to the real sink node.

**Not validated anywhere** — needs hardware with a jack or a bluetooth headset:

- that plugging headphones produces a route change we see at all (slice 2's entire premise);
- the OSD firing on a port change, and the slider icon swapping;
- a multi-port picker (two rows on one card), the availability flip removing a row, and the
  route write actually switching the output.

All of the above are driven by hand-built models in tests. The tests pin *our* logic; they cannot
pin the assumption that PipeWire reports what we think it does.
