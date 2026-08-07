// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A minimal `ext-background-effect-v1` client, for telling a compositor-side blur bug apart from
//! a client-side one.
//!
//! The bug it was written for: dragging a blurred window's edge leaves the blur behind — stale
//! backdrop trailing on grow, a glass pane trailing on shrink. Both a compositor that re-captures
//! too late and a client that respecifies its blur region too late look exactly like that from the
//! outside, and the reporting client (ghost, via a vendored winit) does the size-derived thing.
//!
//! The discriminator is `--region`:
//!
//! * `whole` — one fixed rect far larger than any surface, set **once** and never respecified.
//!   The protocol clips the region to the surface, so this means "all of it" at every size. A
//!   stale region is not *possible* here. If the lag still shows, it is the compositor's.
//! * `exact` — `(0, 0, w, h)` respecified on every configure. What a rounded-corner client must do.
//!   Lag here but not under `whole` puts it in the region-respecify path.
//! * `lag:N` — deliberately the size from N configures ago. Reproduces the client-side bug on
//!   purpose, so we know what it looks like before blaming anything for it.
//! * `none` — no region, i.e. no blur. The control.
//!
//! `--pulse` drives the resize from inside the client, so the whole thing runs without a mouse:
//! the surface oscillates between `--min` and `--max` on its own. That is what makes this drivable
//! headlessly.
//!
//! ```text
//! blur-probe                          # whole-surface region, compositor-driven size
//! blur-probe --region exact --pulse   # self-resizing, region respecified per configure
//! blur-probe --region lag:2 --pulse   # known-bad client, for comparison
//! blur-probe --region whole --pulse --frames 600
//! ```
//!
//! Every frame prints one line, so a run is diffable:
//! `frame 42 configure=(800,600) buffer=(800,600) region=[(0,0,16777216,16777216)] age=41`

use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry::WlRegistry,
    wl_shm::{Format, WlShm}, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{Capability, Event as ManagerEvent, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{Event as XdgSurfaceEvent, XdgSurface},
    xdg_toplevel::{self, Event as ToplevelEvent, XdgToplevel},
    xdg_wm_base::{Event as WmBaseEvent, XdgWmBase},
};

/// A region far larger than any surface. The compositor clips it, so this is "the whole surface"
/// at every size — and, unlike a size-derived rect, it cannot go stale. Kept well below
/// `i32::MAX` so a compositor computing `x + width` in `i32` cannot overflow on it.
const WHOLE_SURFACE: i32 = 1 << 24;

#[derive(Clone, Copy, PartialEq)]
enum RegionMode {
    /// One fixed oversized rect, set once. A stale region is impossible.
    Whole,
    /// `(0, 0, w, h)`, respecified on every configure.
    Exact,
    /// The size from `n` configures ago — the client-side bug, on purpose.
    Lag(usize),
    /// No region at all: no blur. The control arm.
    None,
}

struct Opts {
    region: RegionMode,
    pulse: bool,
    min: (i32, i32),
    max: (i32, i32),
    /// Milliseconds for one full grow-and-shrink cycle.
    period: u64,
    alpha: u8,
    frames: Option<u64>,
    opaque: bool,
    /// Corner radius for the blur region, in surface-local pixels. Non-zero turns the region into a
    /// scanline stack of rects — which is the only way this protocol can express a round corner,
    /// and what winit (so ghost) sends whenever its radii are non-zero.
    radius: i32,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            region: RegionMode::Whole,
            pulse: false,
            min: (480, 360),
            max: (1100, 800),
            period: 3000,
            // Translucent enough that the blurred backdrop is the dominant thing on screen. An
            // *opaque* surface is not just hard to judge: a compositor may cull the effect under
            // it as fully occluded, and then there is nothing to see for a reason that has nothing
            // to do with the bug.
            alpha: 90,
            frames: None,
            opaque: false,
            radius: 0,
        }
    }
}

/// The rects covering a `w`x`h` rounded rectangle, as a scanline stack.
///
/// A `wl_region` is rectangles and nothing else, so a round corner is a staircase — there is no
/// other vocabulary in this protocol. One row per scanline inside the corner band, the rest as a
/// single body rect. This is the shape a rounded-corner client sends, and it is what makes the
/// region multi-rect: the interesting property to test is not the curve but that the compositor
/// masks the blur per-rect rather than to the bounding box.
fn rounded_rect_rects(w: i32, h: i32, radius: i32) -> Vec<(i32, i32, i32, i32)> {
    let r = radius.min(w / 2).min(h / 2);
    if r <= 0 {
        return vec![(0, 0, w, h)];
    }
    let mut rects = Vec::with_capacity(2 * r as usize + 1);
    // The straight middle, full width.
    rects.push((0, r, w, h - 2 * r));
    for i in 0..r {
        // Distance from the corner circle's centre to this scanline's midpoint.
        let dy = (r - i) as f64 - 0.5;
        let dx = ((r as f64).powi(2) - dy * dy).max(0.0).sqrt();
        let inset = ((r as f64) - dx).round() as i32;
        let width = w - 2 * inset;
        if width <= 0 {
            continue;
        }
        rects.push((inset, i, width, 1));
        rects.push((inset, h - 1 - i, width, 1));
    }
    rects
}

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--region" => {
                let v = value()?;
                o.region = match v.as_str() {
                    "whole" => RegionMode::Whole,
                    "exact" => RegionMode::Exact,
                    "none" => RegionMode::None,
                    other => match other.strip_prefix("lag:") {
                        Some(n) => RegionMode::Lag(
                            n.parse().map_err(|_| format!("bad lag count: {n}"))?,
                        ),
                        None => return Err(format!("unknown --region: {other}")),
                    },
                };
            }
            "--pulse" => o.pulse = true,
            "--opaque" => o.opaque = true,
            "--radius" => o.radius = value()?.parse().map_err(|_| "bad --radius")?,
            "--min" => o.min = parse_size(&value()?)?,
            "--max" => o.max = parse_size(&value()?)?,
            "--period" => o.period = value()?.parse().map_err(|_| "bad --period")?,
            "--alpha" => o.alpha = value()?.parse().map_err(|_| "bad --alpha")?,
            "--frames" => o.frames = Some(value()?.parse().map_err(|_| "bad --frames")?),
            "--help" | "-h" => {
                println!("{}", HELP);
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(o)
}

fn parse_size(s: &str) -> Result<(i32, i32), String> {
    let (w, h) = s.split_once('x').ok_or_else(|| format!("bad size: {s} (want WxH)"))?;
    Ok((
        w.parse().map_err(|_| format!("bad width: {w}"))?,
        h.parse().map_err(|_| format!("bad height: {h}"))?,
    ))
}

const HELP: &str = "\
blur-probe — a minimal ext-background-effect-v1 client for isolating blur-lag bugs

  --region whole    one oversized rect, set once; a stale region is impossible (default)
  --region exact    (0,0,w,h) respecified on every configure
  --region lag:N    the size from N configures ago — the client-side bug, on purpose
  --region none     no blur; the control
  --pulse           resize from inside the client, so no mouse is needed
  --min WxH         pulse floor (default 480x360)
  --max WxH         pulse ceiling (default 1100x800)
  --period MS       one grow-and-shrink cycle (default 3000)
  --alpha N         surface alpha, 0-255 (default 90)
  --radius N        round the region's corners: a scanline stack of rects, as a
                    rounded-corner client must send. Only affects exact/lag.
  --opaque          declare an opaque region — expect the effect to be culled
  --frames N        exit after N frames

Read it as: if --region whole lags, the compositor is late re-capturing. If only
--region exact lags, the client is late respecifying. Compare both against lag:N,
which is what a known-late client looks like.";

struct State {
    opts: Opts,
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    wm_base: Option<XdgWmBase>,
    effect_manager: Option<ExtBackgroundEffectManagerV1>,
    capabilities: Capability,

    surface: Option<WlSurface>,
    effect: Option<ExtBackgroundEffectSurfaceV1>,

    /// The size the compositor last configured us to, if it named one.
    configured: Option<(i32, i32)>,
    /// Whether that size is *binding*. In xdg-shell a configure only dictates a size when the
    /// state says so — maximized, fullscreen, tiled, or mid-interactive-resize. Otherwise it is a
    /// suggestion the client may ignore, which is what makes `--pulse` possible at all: without
    /// this the compositor echoes our own size back, we obey it, and the pulse never leaves its
    /// starting size.
    dictated: bool,
    /// Every size we have committed, newest last — what `lag:N` reaches back into.
    history: Vec<(i32, i32)>,
    /// Set when an xdg_surface.configure needs acking before the next commit.
    pending_ack: Option<u32>,
    closed: bool,
    /// Whether the whole-surface region has been sent. It is sent once, on purpose.
    whole_region_sent: bool,
    frame: u64,
    started: Instant,
    last_region: Vec<(i32, i32, i32, i32)>,
}

impl State {
    /// The size to commit this frame.
    fn target_size(&self) -> (i32, i32) {
        // A *binding* size always wins: during an interactive resize it is the whole point, and
        // fighting it would make the probe lie about what it is measuring. A merely suggested one
        // is ignored under --pulse, or the compositor echoing our size back would freeze it.
        let honour = self.configured.filter(|&(w, h)| w > 0 && h > 0);
        if let Some(size) = honour.filter(|_| self.dictated || !self.opts.pulse) {
            return size;
        }
        if !self.opts.pulse {
            return honour.unwrap_or(self.opts.min);
        }
        // Triangle wave, so the turnarounds are the interesting part: grow-then-shrink is where a
        // one-frame-late region is most visible.
        let period = self.opts.period.max(1);
        let phase = (self.started.elapsed().as_millis() as u64 % period) as f64 / period as f64;
        let t = if phase < 0.5 { phase * 2.0 } else { (1.0 - phase) * 2.0 };
        let lerp = |a: i32, b: i32| a + ((b - a) as f64 * t).round() as i32;
        (
            lerp(self.opts.min.0, self.opts.max.0),
            lerp(self.opts.min.1, self.opts.max.1),
        )
    }

    /// The rects the blur region should hold for a surface of `size`, per `--region`.
    fn region_rects(&self, size: (i32, i32)) -> Vec<(i32, i32, i32, i32)> {
        match self.opts.region {
            RegionMode::None => Vec::new(),
            // A radius has no meaning here: the whole point of this arm is a rect that outlives
            // every size, and a rounded one is size-derived by construction.
            RegionMode::Whole => vec![(0, 0, WHOLE_SURFACE, WHOLE_SURFACE)],
            RegionMode::Exact => rounded_rect_rects(size.0, size.1, self.opts.radius),
            RegionMode::Lag(n) => {
                // `history` does not yet contain this frame's size, so index n from the end.
                let (w, h) = self
                    .history
                    .len()
                    .checked_sub(n)
                    .and_then(|i| self.history.get(i))
                    .copied()
                    .unwrap_or(size);
                rounded_rect_rects(w, h, self.opts.radius)
            }
        }
    }
}

fn main() {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("blur-probe: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };

    let conn = Connection::connect_to_env().expect("connect to the Wayland display");
    let (globals, mut queue) = wayland_client::globals::registry_queue_init::<State>(&conn)
        .expect("initial registry roundtrip");
    let qh = queue.handle();

    let mut state = State {
        opts,
        compositor: None,
        shm: None,
        wm_base: None,
        effect_manager: None,
        capabilities: Capability::empty(),
        surface: None,
        effect: None,
        configured: None,
        dictated: false,
        history: Vec::new(),
        pending_ack: None,
        closed: false,
        whole_region_sent: false,
        frame: 0,
        started: Instant::now(),
        last_region: Vec::new(),
    };

    for global in globals.contents().clone_list() {
        match global.interface.as_str() {
            "wl_compositor" => {
                state.compositor = Some(globals.registry().bind(global.name, 4.min(global.version), &qh, ()))
            }
            "wl_shm" => state.shm = Some(globals.registry().bind(global.name, 1, &qh, ())),
            "xdg_wm_base" => {
                state.wm_base = Some(globals.registry().bind(global.name, 1, &qh, ()))
            }
            "ext_background_effect_manager_v1" => {
                state.effect_manager = Some(globals.registry().bind(global.name, 1, &qh, ()))
            }
            _ => {}
        }
    }

    let compositor = state.compositor.clone().expect("no wl_compositor");
    let wm_base = state.wm_base.clone().expect("no xdg_wm_base");
    state.shm.clone().expect("no wl_shm");

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title("blur-probe".into());
    toplevel.set_app_id("blur-probe".into());
    surface.commit();
    state.surface = Some(surface.clone());

    // Learn the capability before deciding whether blur is even on offer.
    queue.roundtrip(&mut state).expect("roundtrip after mapping");

    match (&state.effect_manager, state.capabilities.contains(Capability::Blur)) {
        (None, _) => eprintln!(
            "blur-probe: this compositor does not advertise ext_background_effect_manager_v1 — \
             running without blur, which makes every arm look the same"
        ),
        (Some(_), false) => eprintln!(
            "blur-probe: the compositor advertises the manager but not the Blur capability \
             (capabilities={:?})",
            state.capabilities
        ),
        (Some(manager), true) => {
            // One effect object per surface, for the surface's whole life: a second
            // `get_background_effect` on the same surface is a protocol error.
            state.effect = Some(manager.get_background_effect(&surface, &qh, ()));
        }
    }

    eprintln!(
        "blur-probe: region={} pulse={} alpha={} — one line per frame follows",
        match state.opts.region {
            RegionMode::Whole => "whole".to_string(),
            RegionMode::Exact => "exact".to_string(),
            RegionMode::None => "none".to_string(),
            RegionMode::Lag(n) => format!("lag:{n}"),
        },
        state.opts.pulse,
        state.opts.alpha,
    );

    let mut pool = ShmPool::new();
    while !state.closed {
        if let Some(limit) = state.opts.frames {
            if state.frame >= limit {
                break;
            }
        }

        let size = state.target_size();
        let rects = state.region_rects(size);

        // Order matters, and it is the order winit uses: state the region for the size we are
        // about to show, then attach the buffer that has that size, then commit — so the region
        // and the buffer land in the same atomic commit. A region committed *after* its buffer is
        // exactly the one-frame lag this probe exists to look for.
        if let Some(effect) = &state.effect {
            let resend = match state.opts.region {
                // Sent once, deliberately: re-sending it every frame would hide the very thing
                // this arm is here to prove, namely that no respecify is needed.
                RegionMode::Whole => !state.whole_region_sent,
                RegionMode::None => false,
                _ => rects != state.last_region,
            };
            if resend {
                let region = compositor.create_region(&qh, ());
                for &(x, y, w, h) in &rects {
                    region.add(x, y, w, h);
                }
                effect.set_blur_region(Some(&region));
                // `set_blur_region` copies the region, so the object has no further use.
                region.destroy();
                state.whole_region_sent = true;
                state.last_region = rects.clone();
            }
        }

        let shm = state.shm.clone().unwrap();
        let alpha = state.opts.alpha;
        let (w, h) = (size.0.max(1), size.1.max(1));
        // Wait for an arena the compositor is not still reading. Dispatching is what lets a
        // `release` land, so this cannot be a bare spin.
        let buffer = loop {
            if let Some(buffer) = pool.try_buffer(&shm, &qh, w, h, alpha) {
                break buffer;
            }
            queue
                .blocking_dispatch(&mut state)
                .expect("dispatch while waiting for a buffer release");
        };

        if let Some(serial) = state.pending_ack.take() {
            xdg_surface.ack_configure(serial);
        }

        if state.opts.opaque {
            let region = compositor.create_region(&qh, ());
            region.add(0, 0, size.0, size.1);
            surface.set_opaque_region(Some(&region));
            region.destroy();
        }

        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, size.0.max(1), size.1.max(1));
        surface.commit();

        state.history.push(size);
        // The history only exists to serve `lag:N`; keep it from growing for the whole session.
        if state.history.len() > 64 {
            state.history.remove(0);
        }

        println!(
            "frame {} configure={:?} buffer={}x{} region={:?}",
            state.frame,
            state.configured,
            size.0,
            size.1,
            rects,
        );

        state.frame += 1;
        queue.flush().expect("flush");
        queue
            .blocking_dispatch(&mut state)
            .expect("dispatch the compositor's events");
        // Not a frame callback: the point is a steady, predictable cadence that does not depend on
        // the compositor's pacing, so two arms of the experiment are comparable.
        std::thread::sleep(Duration::from_millis(16));
    }

    eprintln!("blur-probe: {} frames", state.frame);
}

/// How many arenas the pool rotates through. Two is enough to never paint over a buffer the
/// compositor is reading; three keeps the probe from ever *blocking* on a release, which would
/// couple its cadence to the compositor's and make two arms incomparable.
const SLOTS: usize = 3;

/// One shm pool, divided into [`SLOTS`] arenas the client rotates through.
///
/// Painting in place into a single arena is the obvious thing and it is wrong: the compositor may
/// still be reading the buffer it was given last frame, so the repaint lands *inside* a frame it is
/// compositing and the window tears mid-resize. On a probe built to judge whether a blur is a frame
/// behind, that tear is indistinguishable from the bug — the first capture taken with this probe
/// showed the grid changing pitch halfway down the window. Rotate, and only reuse an arena the
/// compositor has released.
struct ShmPool {
    _file: Option<std::fs::File>,
    pool: Option<WlShmPool>,
    /// Bytes per arena; the pool is `SLOTS` times this.
    stride: usize,
    map: Option<*mut u8>,
    /// Shared with each `wl_buffer` as its user data, so `release` can clear it.
    busy: [Busy; SLOTS],
    next: usize,
}

/// Whether the compositor still holds the buffer cut from an arena.
type Busy = std::sync::Arc<std::sync::atomic::AtomicBool>;

impl ShmPool {
    fn new() -> Self {
        Self {
            _file: None,
            pool: None,
            stride: 0,
            map: None,
            busy: std::array::from_fn(|_| Busy::default()),
            next: 0,
        }
    }

    /// Paint `w`x`h` into a free arena and hand back a buffer over it, or `None` if the compositor
    /// is still reading every arena — the caller then dispatches to let a `release` arrive.
    fn try_buffer(
        &mut self,
        shm: &WlShm,
        qh: &QueueHandle<State>,
        w: i32,
        h: i32,
        alpha: u8,
    ) -> Option<WlBuffer> {
        use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
        use rustix::mm::{mmap, MapFlags, ProtFlags};

        let needed = (w as usize) * (h as usize) * 4;
        if needed > self.stride {
            // Over-allocate so a sweep does not re-create the pool on every single step.
            let stride = (needed * 2).next_power_of_two();
            let capacity = stride * SLOTS;
            let fd = memfd_create("blur-probe", MemfdFlags::CLOEXEC).expect("memfd_create");
            ftruncate(&fd, capacity as u64).expect("ftruncate");
            let file = std::fs::File::from(fd);
            let map = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    capacity,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    file.as_fd(),
                    0,
                )
                .expect("mmap")
            };
            if let Some(pool) = self.pool.take() {
                pool.destroy();
            }
            self.pool = Some(shm.create_pool(file.as_fd(), capacity as i32, qh, ()));
            self._file = Some(file);
            self.map = Some(map as *mut u8);
            self.stride = stride;
            // The old arenas are gone; nothing can be holding one.
            self.busy = std::array::from_fn(|_| Busy::default());
        }

        let slot = (0..SLOTS)
            .map(|i| (self.next + i) % SLOTS)
            .find(|&i| !self.busy[i].load(std::sync::atomic::Ordering::Relaxed))?;
        self.next = (slot + 1) % SLOTS;

        let offset = slot * self.stride;
        let map = self.map.expect("mapped pool");
        let pixels = unsafe { std::slice::from_raw_parts_mut(map.add(offset), needed) };
        paint(pixels, w, h, alpha);

        self.busy[slot].store(true, std::sync::atomic::Ordering::Relaxed);
        Some(self.pool.as_ref().unwrap().create_buffer(
            offset as i32,
            w,
            h,
            w * 4,
            Format::Argb8888,
            qh,
            self.busy[slot].clone(),
        ))
    }
}

/// Paint a pattern whose *edges* are unambiguous, because the bug is about edges.
///
/// A flat wash cannot show a lagging blur: the trailing band and the body look identical. So the
/// surface gets a fully opaque 3px frame at the current size and a grid pitched to it — if the
/// blur is a size behind, it stops short of the frame (grow) or runs past it (shrink), and either
/// is visible without measuring anything. Argb8888 is premultiplied, so every channel is scaled by
/// the alpha it ships with.
fn paint(pixels: &mut [u8], w: i32, h: i32, alpha: u8) {
    const BORDER: i32 = 3;
    let a = alpha as u32;
    let premul = |c: u32, a: u32| ((c * a) / 255) as u8;

    for y in 0..h {
        for x in 0..w {
            let edge = x < BORDER || y < BORDER || x >= w - BORDER || y >= h - BORDER;
            let grid = x % 64 == 0 || y % 64 == 0;
            let (r, g, b, a) = if edge {
                // Opaque, so the window's true extent is never in doubt.
                (255u32, 80, 0, 255u32)
            } else if grid {
                (255, 255, 255, a.saturating_add(60).min(255))
            } else {
                (20, 20, 40, a)
            };
            let i = ((y * w + x) * 4) as usize;
            // Argb8888 is 0xAARRGGBB little-endian ⇒ bytes [B, G, R, A].
            pixels[i] = premul(b, a);
            pixels[i + 1] = premul(g, a);
            pixels[i + 2] = premul(r, a);
            pixels[i + 3] = a as u8;
        }
    }
}

impl Dispatch<WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: WmBaseEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let WmBaseEvent::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgSurface,
        event: XdgSurfaceEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let XdgSurfaceEvent::Configure { serial } = event {
            // Acked at commit time, with the buffer that answers it — acking here would
            // acknowledge a size we have not drawn yet.
            state.pending_ack = Some(serial);
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: ToplevelEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ToplevelEvent::Configure { width, height, states } => {
                state.configured = (width > 0 && height > 0).then_some((width, height));
                // `states` is a flat array of little-endian u32 enum values.
                state.dictated = states.chunks_exact(4).any(|s| {
                    let raw = u32::from_ne_bytes([s[0], s[1], s[2], s[3]]);
                    matches!(
                        xdg_toplevel::State::try_from(raw),
                        Ok(xdg_toplevel::State::Maximized)
                            | Ok(xdg_toplevel::State::Fullscreen)
                            | Ok(xdg_toplevel::State::Resizing)
                            | Ok(xdg_toplevel::State::TiledLeft)
                            | Ok(xdg_toplevel::State::TiledRight)
                            | Ok(xdg_toplevel::State::TiledTop)
                            | Ok(xdg_toplevel::State::TiledBottom)
                    )
                });
            }
            ToplevelEvent::Close => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        event: ManagerEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ManagerEvent::Capabilities { flags } = event {
            // The capability is live: it arrives on bind and again whenever it changes.
            if let WEnum::Value(flags) = flags {
                state.capabilities = flags;
            }
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore WlRegion);
delegate_noop!(State: ignore WlShm);
delegate_noop!(State: ignore WlShmPool);
impl Dispatch<WlBuffer, Busy> for State {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        busy: &Busy,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            busy.store(false, std::sync::atomic::Ordering::Relaxed);
            // The buffer named a size we will not draw again — every frame has its own. Holding it
            // would leak one object per frame for the length of the run.
            buffer.destroy();
        }
    }
}
delegate_noop!(State: ignore ExtBackgroundEffectSurfaceV1);
