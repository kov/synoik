// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Every draw of every frame, in order: which target it went into, in which render pass of that
//! target, what material, where and how much (`SYNOIK_FRAME_LOG=…,ledger`).
//!
//! It answers the one question the frame log's totals cannot: *was this draw recorded at all, and
//! if so after which pass boundary?* A translucent, blurred surface can lose its whole contribution
//! to a frame on the glass — backdrop, tint and window alike — with the layer underneath showing
//! through. A draw that was never recorded is our bug; a draw that was recorded and still did not
//! land is downstream of us. The frame log records neither: it counts draws, it does not list them.
//!
//! The pass index is there because a backdrop capture ends the render pass and begins a
//! continuation (`VulkanFrame::capture_region`), and that boundary is where "everything drawn after
//! it" becomes a set with a name. Matched against a frame stamped on the glass
//! ([`crate::render_helpers::frame_stamp`]), the ledger says whether what went missing is exactly
//! what one target drew after one split.
//!
//! Recording is per thread and keyed by the frame log's own `begin`/`end`, so draws outside a
//! logged frame (a bake between frames) are not recorded, and nothing here is shared between the
//! test threads of one binary. A frame's events are plain values in a recycled `Vec`: the frame
//! path formats nothing and, once the ring is full, allocates nothing.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt::Write as _;

use smithay::utils::{Physical, Rectangle};

/// How many frames the ledger keeps by default: 40 s at 60 Hz on one output. Long enough to see a
/// blink, find it in a recording and send `SIGUSR1`. At ~250 draws a frame that is ~30 MB.
pub const DEFAULT_FRAMES: usize = 2400;

/// What a draw drew. A texture carries the raw handle of the image it sampled, which is what joins
/// an offscreen's own frame to the draw that composites it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Material {
    Texture(u64),
    ClippedTexture(u64),
    RoundedTexture(u64),
    GradientFade(u64),
    /// A backdrop composite, or any other postprocess of a captured texture.
    Postprocess(u64),
    Resize(u64),
    CustomResize(u64),
    CustomAnim(u64),
    Solid,
    ClippedSolid,
    RoundedRect,
    Triangle,
    Glyphs,
    Border,
    Shadow,
    DropShadow,
    /// `Frame::clear`: pixels written without a draw call.
    Clear,
    /// The copy of the drawn regions out of a present-blit shadow into the scanout buffer it
    /// names — the last step before the display engine reads it. An empty copy is recorded too:
    /// it means the scanout buffer kept its old contents everywhere.
    PresentBlit(u64),
}

impl Material {
    fn label(self) -> (&'static str, Option<u64>) {
        match self {
            Material::Texture(i) => ("texture", Some(i)),
            Material::ClippedTexture(i) => ("clipped-texture", Some(i)),
            Material::RoundedTexture(i) => ("rounded-texture", Some(i)),
            Material::GradientFade(i) => ("gradient-fade", Some(i)),
            Material::Postprocess(i) => ("postprocess", Some(i)),
            Material::Resize(i) => ("resize", Some(i)),
            Material::CustomResize(i) => ("custom-resize", Some(i)),
            Material::CustomAnim(i) => ("custom-anim", Some(i)),
            Material::Solid => ("solid", None),
            Material::ClippedSolid => ("clipped-solid", None),
            Material::RoundedRect => ("rounded-rect", None),
            Material::Triangle => ("triangle", None),
            Material::Glyphs => ("glyphs", None),
            Material::Border => ("border", None),
            Material::Shadow => ("shadow", None),
            Material::DropShadow => ("drop-shadow", None),
            Material::Clear => ("clear", None),
            Material::PresentBlit(i) => ("present-blit", Some(i)),
        }
    }
}

/// One thing that happened to a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A `VulkanFrame` began on `image`. Scanout or offscreen, and whether its first pass loads
    /// the previous contents (LOAD) or discards them (DONT_CARE).
    Target {
        image: u64,
        offscreen: bool,
        preserve: bool,
        w: u32,
        h: u32,
    },
    /// A backdrop capture ended the target's render pass, blitted `src` into a `w`×`h` capture and
    /// began the continuation pass. Every later draw into this target is in the next pass.
    Split {
        src: Rectangle<i32, Physical>,
        w: u32,
        h: u32,
    },
    /// A draw: its material, the bounding box of its scissor rects, how many there were and how
    /// many pixels they cover.
    Draw {
        material: Material,
        bbox: Rectangle<i32, Physical>,
        rects: u32,
        area: u64,
    },
    /// A draw or a capture that returned without recording anything, and why.
    Skipped(&'static str),
    /// The target's frame finished: its command buffer was ended and submitted, unless an error
    /// came first (the journal says so when one does).
    Finished,
}

#[derive(Debug, Default)]
struct Frame {
    seq: u64,
    output: String,
    /// `(target ordinal, pass within that target, event)`.
    events: Vec<(u16, u16, Event)>,
}

#[derive(Debug, Default)]
struct Ledger {
    /// Frames to keep; 0 = off.
    cap: usize,
    frames: VecDeque<Frame>,
    /// The frame being built, if a logged frame is in flight.
    current: Option<Frame>,
    /// Targets begun and not yet submitted, innermost last: `(ordinal, pass)`.
    stack: Vec<(u16, u16)>,
    /// How many targets the current frame has begun.
    targets: u16,
}

thread_local! {
    static LEDGER: RefCell<Ledger> = RefCell::new(Ledger::default());
}

/// Keep the last `frames` frames on this thread; 0 turns the ledger off and drops what it held.
pub fn set_capacity(frames: usize) {
    LEDGER.with_borrow_mut(|l| {
        l.cap = frames;
        if frames == 0 {
            *l = Ledger::default();
        }
    });
}

/// Start recording frame `seq` on `output`. Called by the frame log's `begin`.
pub fn begin(seq: u64, output: &str) {
    LEDGER.with_borrow_mut(|l| {
        if l.cap == 0 {
            return;
        }
        // Reuse the oldest frame's allocation once the ring is full.
        let mut frame = if l.frames.len() >= l.cap {
            l.frames.pop_front().unwrap_or_default()
        } else {
            Frame::default()
        };
        frame.seq = seq;
        frame.output.clear();
        frame.output.push_str(output);
        frame.events.clear();
        l.current = Some(frame);
        l.stack.clear();
        l.targets = 0;
    });
}

/// Bank the frame being recorded. Called by the frame log's `end`.
pub fn end() {
    LEDGER.with_borrow_mut(|l| {
        if let Some(frame) = l.current.take() {
            l.frames.push_back(frame);
        }
        l.stack.clear();
    });
}

fn push(l: &mut Ledger, event: Event) {
    let Some(frame) = l.current.as_mut() else {
        return;
    };
    let (target, pass) = l.stack.last().copied().unwrap_or((u16::MAX, 0));
    frame.events.push((target, pass, event));
}

/// A `VulkanFrame` began. See [`Event::Target`].
pub fn target(image: u64, offscreen: bool, preserve: bool, w: u32, h: u32) {
    LEDGER.with_borrow_mut(|l| {
        if l.current.is_none() {
            return;
        }
        let ordinal = l.targets;
        l.targets = l.targets.saturating_add(1);
        l.stack.push((ordinal, 0));
        push(
            l,
            Event::Target {
                image,
                offscreen,
                preserve,
                w,
                h,
            },
        );
    });
}

/// The innermost target's pass was split by a capture. See [`Event::Split`].
pub fn split(src: Rectangle<i32, Physical>, w: u32, h: u32) {
    LEDGER.with_borrow_mut(|l| {
        if l.current.is_none() {
            return;
        }
        push(l, Event::Split { src, w, h });
        if let Some(top) = l.stack.last_mut() {
            top.1 = top.1.saturating_add(1);
        }
    });
}

/// A draw into the innermost target, covering `scissors`.
pub fn draw(material: Material, scissors: impl Iterator<Item = Rectangle<i32, Physical>>) {
    LEDGER.with_borrow_mut(|l| {
        if l.current.is_none() {
            return;
        }
        let mut bbox: Option<Rectangle<i32, Physical>> = None;
        let mut rects = 0u32;
        let mut area = 0u64;
        for r in scissors {
            rects += 1;
            area += (r.size.w.max(0) as u64) * (r.size.h.max(0) as u64);
            bbox = Some(bbox.map_or(r, |b| b.merge(r)));
        }
        push(
            l,
            Event::Draw {
                material,
                bbox: bbox.unwrap_or_default(),
                rects,
                area,
            },
        );
    });
}

/// Something that could have drawn returned without recording, for `reason`.
pub fn skipped(reason: &'static str) {
    LEDGER.with_borrow_mut(|l| push(l, Event::Skipped(reason)));
}

/// The innermost target finished; draws after this belong to the one it was nested in.
pub fn finished() {
    LEDGER.with_borrow_mut(|l| {
        if l.current.is_none() {
            return;
        }
        push(l, Event::Finished);
        l.stack.pop();
    });
}

/// Whether the ledger is recording on this thread.
pub fn is_enabled() -> bool {
    LEDGER.with_borrow(|l| l.cap > 0)
}

fn rect(r: Rectangle<i32, Physical>) -> String {
    format!("{},{} {}x{}", r.loc.x, r.loc.y, r.size.w, r.size.h)
}

/// Format banked frames, oldest first, plus the one in flight. One header line per frame, one
/// indented line per event, prefixed `t<target>.p<pass>`.
pub fn format() -> String {
    LEDGER.with_borrow(|l| {
        let mut out = String::new();
        for frame in l.frames.iter().chain(l.current.as_ref()) {
            let _ = writeln!(out, "seq {} on {}", frame.seq, frame.output);
            for (target, pass, event) in &frame.events {
                let at = if *target == u16::MAX {
                    "t-.p-".to_owned()
                } else {
                    format!("t{target}.p{pass}")
                };
                let _ = match event {
                    Event::Target {
                        image,
                        offscreen,
                        preserve,
                        w,
                        h,
                    } => writeln!(
                        out,
                        "  {at} target {} {w}x{h} image={image:#x} {}",
                        if *offscreen { "offscreen" } else { "scanout" },
                        if *preserve { "load" } else { "discard" },
                    ),
                    Event::Split { src, w, h } => {
                        writeln!(out, "  {at} split capture {} -> {w}x{h}", rect(*src))
                    }
                    Event::Draw {
                        material,
                        bbox,
                        rects,
                        area,
                    } => {
                        let (name, src) = material.label();
                        let src = src.map(|i| format!(" src={i:#x}")).unwrap_or_default();
                        writeln!(
                            out,
                            "  {at} draw {name}{src} bbox={} rects={rects} area={area}",
                            rect(*bbox)
                        )
                    }
                    Event::Skipped(reason) => writeln!(out, "  {at} skipped {reason}"),
                    Event::Finished => writeln!(out, "  {at} finished"),
                };
            }
        }
        out
    })
}

/// The events of the frame with sequence number `seq`, for tests.
#[cfg(test)]
pub fn events_of(seq: u64) -> Option<Vec<(u16, u16, Event)>> {
    LEDGER.with_borrow(|l| {
        l.frames
            .iter()
            .chain(l.current.as_ref())
            .find(|f| f.seq == seq)
            .map(|f| f.events.clone())
    })
}

/// The sequence numbers the ledger holds, oldest first, for tests.
#[cfg(test)]
pub fn seqs() -> Vec<u64> {
    LEDGER.with_borrow(|l| l.frames.iter().map(|f| f.seq).collect())
}

#[cfg(test)]
mod tests {
    use smithay::utils::{Point, Size};

    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Physical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn draws_carry_their_target_and_the_pass_a_split_opened() {
        set_capacity(4);
        begin(7, "out");
        target(0xa, false, true, 100, 100);
        draw(Material::Solid, [r(0, 0, 10, 10)].into_iter());
        // A nested offscreen, drawn and submitted inside the output's frame.
        target(0xb, true, false, 50, 50);
        draw(
            Material::Texture(0xc),
            [r(0, 0, 5, 5), r(10, 0, 5, 5)].into_iter(),
        );
        finished();
        split(r(0, 0, 20, 20), 10, 10);
        draw(Material::Postprocess(0xd), [r(0, 0, 20, 20)].into_iter());
        finished();
        end();

        let events = events_of(7).expect("the frame was banked");
        let draws: Vec<_> = events
            .iter()
            .filter_map(|(t, p, e)| match e {
                Event::Draw {
                    material,
                    bbox,
                    rects,
                    area,
                } => Some((*t, *p, *material, *bbox, *rects, *area)),
                _ => None,
            })
            .collect();
        assert_eq!(
            draws,
            vec![
                (0, 0, Material::Solid, r(0, 0, 10, 10), 1, 100),
                (1, 0, Material::Texture(0xc), r(0, 0, 15, 5), 2, 50),
                (0, 1, Material::Postprocess(0xd), r(0, 0, 20, 20), 1, 400),
            ]
        );
        set_capacity(0);
    }

    #[test]
    fn the_ring_keeps_the_newest_frames_and_nothing_outside_a_frame() {
        set_capacity(2);
        draw(Material::Solid, [r(0, 0, 1, 1)].into_iter());
        for seq in 1..=3 {
            begin(seq, "out");
            target(1, false, true, 1, 1);
            finished();
            end();
        }
        assert_eq!(seqs(), vec![2, 3]);
        set_capacity(0);
        assert!(!is_enabled());
    }
}
