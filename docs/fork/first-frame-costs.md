# Cold costs: finding the work we only ever do once

**Status:** plan, not built. Written 2026-07-27 out of the live session that produced
[`present-misses.md`](./present-misses.md).

## Cold icon keys, and a prewarm that ran too early

Two flickers reported from the seat on 2026-08-01 — the lock screen's avatar on the first unlock
prompt, and app-grid icons "sometimes" on first open. Same class, two unrelated mechanisms, and
both are the shape this document predicts: **a cold icon key draws nothing at all.**
`IconCache::texture` hands a miss to the worker and returns whatever `stale_textures` holds, which
is empty until an icon-theme change (`render_helpers/icon.rs:287-298`), so the element is simply
not emitted for those frames.

**The avatar: bucketing turned one cold miss into twelve.** The prompt page scales 0.3 → 1 over the
300 ms crossfade, and the avatar was requested at `rest_px * page_scale`, bucketed to sixteenths so
the cache would not be thrashed. Bucketing bounds the *steady-state* key count and does nothing for
the cold run: the first crossfade asked for eleven distinct sizes, each its own cold miss, each
drawing no glyph while the plate underneath kept drawing. The fix is the one `window_preview`
already uses for the same problem — ask at a fixed size, scale on the GPU — now a reusable
`widget::icon_element_scaled`. Pinned by `the_crossfade_asks_for_one_avatar_not_one_per_frame`,
which counts requests through `wire_test_worker`; with the old code it reports eleven.

**The app grid: the prewarm ran before the worker existed.** `prewarm_app_icons` returns early
without a decode worker, and its only startup caller was `add_output` — which on a TTY seat runs
from `backend.init`, tens of lines *before* `spawn_worker`. So it warmed nothing, every tile
decoded lazily on the frame it first appeared, and whether you saw it depended on whether some
later settings change or catalog reload had warmed the cache first. That is the "sometimes". Now
warmed again once the worker is up.

**The general gap this leaves:** there is still no prewarm of any kind for `IconCache` (symbolic).
Every first-ever panel, quick-settings and calendar icon has a one-frame absence today; it is
invisible only because those surfaces are static, so the missing frame is the first frame and there
is nothing to flicker *from*. Any new animated surface drawing a symbolic icon will hit it.

**Method note.** Neither bug is visible to a test that does not ask for the async miss path: with
no worker the cache rasterizes inline and every miss draws fine. `IconCache::wire_test_worker`
exists for this, and the app-grid ordering bug is *still* not covered — it needs the real TTY init
sequence, which the headless fixture does not run.


## The problem

A cost paid **once per process, or once per surface**, is invisible to every instrument we have.
The frame log's summary reports p50/p95 and "N over budget" over a 10 s window; a single 58 ms
frame at login is one sample in a few hundred and disappears. Steady-state is what we optimise
and steady-state is fine — the seat's idle frame is 0.6 ms and 0.3% of frames go over budget.

But the user does not experience the median. They experience the login hitch, the first Super
press, the first *show-apps*. Every one of those is a first-execution cost, and we currently find
them one at a time, by eye, out of a journal.

We have just done exactly that once and it worked: the first frame spent **32.91 ms shaping four
runs** against 0.34 ms for two runs on every later frame, and the fix
([`2531d8d3`](#)) was to read the fonts on a startup thread. That is one site. There is no reason
to think it is the only one, and no reason to keep finding them by hand.

## What we already know is in this class

From one 14-minute session (`niri[380336]`, 2026-07-27), the whole set of over-budget frames:

| when | total | where it went | status |
|---|---|---|---|
| first frame ever | 57.81 ms | `collect 48.83ms` = **32.91 ms shaping** + 10.33 ms creating 9 images + 4.09 ms panel bake | shaping **fixed**; the rest open |
| first overview open | 24.75 ms | `collect 15.95ms`, of which only ~5 ms is attributed (1.27 created + 1.44 bakes + 2.27 shaping) | **~11 ms unexplained** |
| first *show-apps* | 19.43 ms | 446 draws, 26 images created in 6.23 ms, 3.1 MiB uploaded, app-grid bake 2.97 ms | open |
| first dash draw | 16.99 ms | `ui/dash.rs:483` bake 3.29 ms + 11.4 ms GPU | open |

None of these recurred in a second, larger burst of the same interactions later in the session
(1655 frames, exactly one `collect` above 4 ms — and that one was a single 11.87 ms image
allocation, a different problem that belongs to the host, see `present-misses.md` §7).

So: four known sites, one fixed, and no way to enumerate the rest.

## The plan

Two instruments, then a pass over what they find. The second one is the interesting half.

### Slice 1 — close the attribution gap in `collect`

`collect` currently reports a total, and separately reports bakes, image creations, staging
writes, uploads and shaped runs. Everything else is a **residual you have to compute by hand**,
which is how ~11 ms sat unnoticed in the first-overview frame.

- ~~Print the residual explicitly~~ — **DONE (`f0a17bb1`)**. Any phase now reports what it cannot
  explain: `collect 15.95ms (11.00ms unattributed)`. The residual is a **union** of the timed
  scopes, not a sum of the buckets: the buckets nest (a bake allocates and shapes inside itself),
  so a sum double-counts and would read zero residual on exactly the frames that have one. A phase
  with no buckets at all stays quiet. Floor 0.5 ms.
- Add scoped timers inside `Synoik::render_to_vec` at the boundaries that already exist — per UI
  surface (panel, dash, app grid, search, notifications), and around icon resolution. **Not done**
  — deliberately waiting for a validation run on the new binary to say *where* a residual actually
  shows up, rather than instrumenting six surfaces on the strength of one frame from before the
  residual was measurable.

Deliverable: no frame line with more than ~1 ms of unattributed collect.

### Slice 2 — a first-execution registry

This is the part that generalises. The costs above share one structural property: they are the
**first** run of a code path in the process. That is detectable without knowing in advance where
they are.

A `cold::timed()` guard, keyed by `#[track_caller]` `Location` exactly as the bake attribution
already is (`src/frame_log.rs`, `BAKE_SITES`), recording per site:

```
site                              calls   first      median    excess
ui/app_grid.rs:783                   41   2.97ms     0.04ms    2.93ms
synoik-vk/text.rs (shape)            1204   8.20ms     0.17ms    8.03ms
...
```

`excess = first − median`, and the report is sorted by it. A site whose first call costs what
every other call costs has nothing to smooth; a site with a large excess is a hitch with a name.

Two design notes that matter:

- **Sample the whole warm distribution, not the second call.** The second call may still be cold
  in a different way (a second glyph size, a second icon theme lookup). Median over all calls.
- **Zero cost when off**, like the rest of the frame log — the registry only records when
  `SYNOIK_FRAME_LOG` is set.

Emitted on demand rather than on a timer: a `synoik msg` request that dumps the table, so it can be
read after deliberately exercising a surface for the first time.

Deliverable: a ranked list of first-execution costs across a session, obtained without knowing
where to look.

### Slice 3 — smooth what slices 1 and 2 name

We already have three working shapes for this, and every site should map onto one of them:

| pattern | precedent |
|---|---|
| **prewarm on a thread at startup** | fonts (`2531d8d3`), app icons (`4eea62e4`) |
| **move the work off the frame thread entirely** | wallpaper decode worker (`4504c5b5`), async icon decode (`e7a1c2ed`) |
| **do it eagerly at construction instead of lazily on first use** | the Vulkan pipelines, all built in `VulkanRenderer::new` |

GNOME's own answer is the same one: `AppDisplay` is resident from session start and populated off
the idle deferred-work queue into a `POLICY_FOREVER` cache (`appDisplay.js:1339`,
`st-texture-cache.c:998`), which is why its first grid open never shows a blank tile. Where a site
resists all three, the fallback is to make it incremental rather than one blocking chunk.

### Nice to have — a font database cache on disk

Not scheduled; the last drop of juice once the named sites are smoothed.

Every launch, `FontSystem::new()` walks the system font directories and parses each face's metadata
to build the family index. That is the ~250–400 ms cold cost the prewarm currently *hides* rather
than removes — and hiding it is why `race_to_init` exists (`d4742f8c`): the work is still on some
thread, still competing for the same startup window, and we pay it in full on every login.

GNOME does not pay it, and not because Pango is faster: **fontconfig keeps the enumeration in an
on-disk cache** (`fc-cache`, `/var/cache/fontconfig` + `~/.cache/fontconfig`), rebuilt only when a
font directory's mtime changes. Startup is a cache read, not a scan. `fontdb` has no equivalent —
it rescans every process.

The shape of the fix: serialize the resolved face index (path, index, family, weight, style,
stretch — not the font data) into `$XDG_CACHE_HOME/gnome-shell-rs/fonts.<version>.bin`, keyed on
the font directories' mtimes plus a format version, and load faces lazily by path on first use.
Invalidate wholesale on any mtime change; a stale-cache bug shows as a missing family, so
correctness must come from the key, never from patching entries. Worth measuring the split first —
if most of the cost is the raw file reads rather than the parse, a cache of the index alone buys
less than it looks and the answer is to load fewer faces instead.

Related: this is also the thing that would make the prewarm thread unnecessary rather than
load-bearing, which is a simplification worth having on its own.

## The methodology trap, recorded

Measuring a cold cost twice in one process gives you the **warm** number, and the difference can
be two orders of magnitude. The font probe read **408 ms** for the first `measure_line_width` and
**11.8 ms** for the same call on the next run of the same test — that swing is the kernel page
cache, not the code. It nearly produced a confident and wrong writeup ("we build the font database
twice, 400 ms each").

So any harness for this must:

1. measure the first occurrence in a **fresh process**, and
2. distinguish I/O-bound cold costs (page cache; drop caches to reproduce honestly) from
   compute-bound ones (first parse, first compile), because only the second kind is fixed by
   prewarming *in* the process — the first kind is fixed by touching the *files* early, which is
   why the font fix does one shape rather than just constructing a `FontSystem`.

## Scope

Explicitly **not** in this plan: the bimodal image-allocation spikes (p50 0.10 ms, max 23.38 ms
for a single `vkCreateImage`). Those are not first-execution costs — one hit an idle desktop
twelve minutes into the session — and they are host-side. They live in
[`present-misses.md`](./present-misses.md) §7.
