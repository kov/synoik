// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The frame log's sequence number, drawn into every corner of every output frame
//! (`SYNOIK_FRAME_LOG=…,stamp`).
//!
//! It exists to join two records that otherwise cannot be joined: a **host-side** screen recording,
//! which is the only instrument that sees what reached the glass, and the frame log's record of the
//! frame the compositor believes it drew there. Nothing inside the guest can stand in for the
//! recording — every capture path re-renders the scene, so a frame that lost pixels on the way to
//! the screen photographs clean. And the recording cannot be joined by time: the recorder samples
//! at a variable rate, well under the display's, and drops frames. So each frame carries its own
//! number, and the recording is read, not counted.
//!
//! It is also a canary. The stamp is pushed first, so it is the topmost element and the **last**
//! draw of the frame — after every backdrop capture has split the render pass. A corner whose stamp
//! is missing from a recorded frame lost what the frame drew last in that part of the screen.
//!
//! # Layout
//!
//! One row of [`CELL_PX`]-square cells on a black backing one cell larger on every side:
//!
//! ```text
//! [white][black][d21 … d0][parity][white]
//! ```
//!
//! Data bits are MSB first, white = 1. The parity cell makes the count of white cells among the
//! data and parity even. Physical pixels, not logical, so the cells are the same size on the glass
//! at every scale. Pure black and white because a video codec keeps luma and throws chroma away.
//!
//! The four copies sit flush in the output's four corners. [`decode`] reads one back.
//!
//! **It changes what it measures, a little:** every frame the stamp draws now has damage, so a
//! redraw that would have found nothing to paint presents a frame instead. The overview, where
//! this is aimed, already repaints the whole output every frame.

use std::cell::RefCell;

use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::Color32F;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use crate::render_helpers::solid_color::SolidColorRenderElement;

/// One cell's side, in physical pixels.
pub const CELL_PX: i32 = 24;
/// How many low bits of the sequence number the stamp carries: ~19 hours at 60 Hz.
pub const DATA_BITS: u32 = 22;
/// Cells in the coded row: the white/black lead-in, the data, the parity, the white end.
const ROW_CELLS: i32 = 2 + DATA_BITS as i32 + 2;
/// The backing's size in cells, one cell of black around the row.
const BACKING_CELLS: (i32, i32) = (ROW_CELLS + 2, 3);

/// The stamp's footprint in physical pixels.
pub const fn size_px() -> (i32, i32) {
    (BACKING_CELLS.0 * CELL_PX, BACKING_CELLS.1 * CELL_PX)
}

/// The row's cells, left to right, white = `true`.
fn row(seq: u64) -> [bool; ROW_CELLS as usize] {
    let data = seq & ((1 << DATA_BITS) - 1);
    let mut cells = [false; ROW_CELLS as usize];
    cells[0] = true;
    for bit in 0..DATA_BITS {
        cells[2 + bit as usize] = (data >> (DATA_BITS - 1 - bit)) & 1 == 1;
    }
    cells[2 + DATA_BITS as usize] = data.count_ones() % 2 == 1;
    cells[ROW_CELLS as usize - 1] = true;
    cells
}

thread_local! {
    /// One stable `Id` per element slot, so the damage tracker sees the same stamp change rather
    /// than four fresh ones appearing and four old ones vanishing every frame.
    static IDS: RefCell<Vec<Id>> = const { RefCell::new(Vec::new()) };
}

fn id(slot: usize) -> Id {
    IDS.with_borrow_mut(|ids| {
        while ids.len() <= slot {
            ids.push(Id::new());
        }
        ids[slot].clone()
    })
}

/// Push the stamp for frame `seq` into all four corners of an output of `output_size` (logical) at
/// `scale`.
pub fn render(
    seq: u64,
    output_size: Size<f64, Logical>,
    scale: f64,
    push: &mut dyn FnMut(SolidColorRenderElement),
) {
    let (w_px, h_px) = size_px();
    let physical = output_size.to_physical_precise_round::<_, i32>(scale);
    let commit = CommitCounter::from(seq as usize);
    let rect = |loc: (i32, i32), size: (i32, i32)| {
        Rectangle::<i32, Physical>::new(Point::from(loc), Size::from(size))
            .to_f64()
            .to_logical(scale)
    };
    let cells = row(seq);
    let per_corner = 1 + cells.len();

    for (corner, (x, y)) in corners(physical.w, physical.h).into_iter().enumerate() {
        let base = corner * per_corner;
        // White cells first: first pushed is topmost.
        for (i, _) in cells.iter().enumerate().filter(|(_, white)| **white) {
            push(SolidColorRenderElement::new(
                id(base + 1 + i),
                rect(
                    (x + (1 + i as i32) * CELL_PX, y + CELL_PX),
                    (CELL_PX, CELL_PX),
                ),
                commit,
                Color32F::from([1., 1., 1., 1.]),
                Kind::Unspecified,
            ));
        }
        push(SolidColorRenderElement::new(
            id(base),
            rect((x, y), (w_px, h_px)),
            commit,
            Color32F::from([0., 0., 0., 1.]),
            Kind::Unspecified,
        ));
    }
}

/// Read a stamp whose backing's top-left corner is at `origin` in an RGBA image `w` pixels wide.
///
/// Samples each cell at its centre and thresholds its luma at mid-grey, so it survives a lossy
/// video frame. `None` when the lead-in, the end cell or the parity is wrong — which is what a
/// stamp partly or wholly missing from the frame reads as.
pub fn decode(pixels: &[u8], w: i32, origin: (i32, i32)) -> Option<u64> {
    let h = pixels.len() as i32 / 4 / w;
    let white = |cell: i32| -> Option<bool> {
        let x = origin.0 + (1 + cell) * CELL_PX + CELL_PX / 2;
        let y = origin.1 + CELL_PX + CELL_PX / 2;
        if x < 0 || y < 0 || x >= w || y >= h {
            return None;
        }
        let i = ((y * w + x) * 4) as usize;
        let luma = 0.299 * f64::from(pixels[i])
            + 0.587 * f64::from(pixels[i + 1])
            + 0.114 * f64::from(pixels[i + 2]);
        Some(luma > 127.5)
    };

    if !white(0)? || white(1)? || !white(ROW_CELLS - 1)? {
        return None;
    }
    let mut data = 0u64;
    for bit in 0..DATA_BITS as i32 {
        data = (data << 1) | u64::from(white(2 + bit)?);
    }
    let parity = white(2 + DATA_BITS as i32)?;
    (parity == (data.count_ones() % 2 == 1)).then_some(data)
}

/// Where each corner's stamp starts in an output `w`×`h` physical pixels: top-left, top-right,
/// bottom-left, bottom-right.
pub fn corners(w: i32, h: i32) -> [(i32, i32); 4] {
    let (sw, sh) = size_px();
    [(0, 0), (w - sw, 0), (0, h - sh), (w - sw, h - sh)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paint `row(seq)` into an RGBA buffer the way the elements lay it out, and read it back.
    fn painted(seq: u64) -> (Vec<u8>, i32) {
        let (w, h) = size_px();
        let mut px = vec![0u8; (w * h * 4) as usize];
        for (i, white) in row(seq).iter().enumerate() {
            if !white {
                continue;
            }
            for y in CELL_PX..2 * CELL_PX {
                for x in (1 + i as i32) * CELL_PX..(2 + i as i32) * CELL_PX {
                    let o = ((y * w + x) * 4) as usize;
                    px[o..o + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        (px, w)
    }

    #[test]
    fn a_stamp_reads_back_its_sequence_number() {
        for seq in [1, 2, 0b1010_1010, 12345, (1 << DATA_BITS) - 1] {
            let (px, w) = painted(seq);
            assert_eq!(decode(&px, w, (0, 0)), Some(seq));
        }
        // Only the low bits travel.
        let (px, w) = painted((1 << DATA_BITS) + 7);
        assert_eq!(decode(&px, w, (0, 0)), Some(7));
    }

    #[test]
    fn a_damaged_stamp_reads_as_none_not_as_another_number() {
        let (mut px, w) = painted(12345);
        // Blacken one data cell that was white: the parity no longer holds.
        let cell = row(12345).iter().skip(2).position(|w| *w).unwrap() as i32 + 2;
        for y in CELL_PX..2 * CELL_PX {
            for x in (1 + cell) * CELL_PX..(2 + cell) * CELL_PX {
                let o = ((y * w + x) * 4) as usize;
                px[o..o + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        assert_eq!(decode(&px, w, (0, 0)), None);
        // An all-black stamp (a missing one) has no lead-in.
        let (w, h) = size_px();
        assert_eq!(decode(&vec![0u8; (w * h * 4) as usize], w, (0, 0)), None);
    }
}
