// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The firmware's boot logo, as the kernel exports ACPI's Boot Graphics Resource Table
//! (`docs/fork/boot-splash.md` §4.1): the image the firmware drew, and where it drew it. Read as
//! two-step reads it (plymouth 24.004.60, `src/plugins/splash/two-step/plugin.c`
//! `get_bgrt_sysfs_info`; the image through `src/libply-splash-graphics/ply-image.c`
//! `ply_image_load_bmp`).

use std::path::Path;
use std::{fs, io};

use crate::layout::{Point, Size};
use crate::paint::Image;

/// Where the kernel exports the table.
pub const DIR: &str = "/sys/firmware/acpi/bgrt";

/// The status field's orientation bits (ACPI 6.2): set, the image is not pre-rotated.
const ORIENTATION_MASK: u32 = 3 << 1;

/// The logo, and its top-left on the screen the firmware drew it on, in that screen's pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logo {
    pub image: Image,
    pub offset: Point,
}

impl Logo {
    /// The logo the table in `dir` describes. `None` without one, which is every machine that
    /// boots on a device tree, and for one the splash cannot draw.
    pub fn load(dir: &Path) -> Option<Self> {
        if !dir.exists() {
            return None;
        }
        Self::read(dir)
            .map_err(|err| warn!("splash: firmware logo: {err}"))
            .ok()
            .flatten()
    }

    fn read(dir: &Path) -> io::Result<Option<Self>> {
        let number = |name: &str| -> io::Result<i64> {
            let text = fs::read_to_string(dir.join(name))?;
            text.trim()
                .parse()
                .map_err(|err| io::Error::other(format!("{name}: {err}")))
        };
        let status = number("status")?;
        // An image the firmware wants rotated is meant for a rotated panel. two-step draws it only
        // when the panel is rotated the same way, and the splash never rotates a panel.
        if status as u32 & ORIENTATION_MASK != 0 {
            info!("splash: the firmware logo is for a rotated panel, leaving it out");
            return Ok(None);
        }
        let offset = Point::from((number("xoffset")? as i32, number("yoffset")? as i32));
        let image = decode_bmp(&fs::read(dir.join("image"))?)?;
        let (size, at) = (image.size, offset);
        info!(
            "splash: firmware logo {}x{} at {}x{}",
            size.w, size.h, at.x, at.y
        );
        Ok(Some(Self { image, offset }))
    }
}

/// Decode a BMP as plymouth does: 24-bit, uncompressed, with a 40-byte header; rows bottom-up
/// unless the height is negative. Anything else is refused, as plymouth refuses it.
pub fn decode_bmp(bytes: &[u8]) -> io::Result<Image> {
    let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    let u16_at = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| invalid("truncated header"))
    };
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
            .ok_or_else(|| invalid("truncated header"))
    };
    if u16_at(0)? != 0x4d42 {
        return Err(invalid("not a BMP"));
    }
    let bits_at = u32_at(10)? as usize;
    let (header, width, height) = (u32_at(14)?, u32_at(18)? as i32, u32_at(22)? as i32);
    let (planes, bpp, compression) = (u16_at(26)?, u16_at(28)?, u32_at(30)?);
    if header != 40 || width < 0 || planes != 1 || bpp != 24 || compression != 0 {
        return Err(invalid("not a 24-bit uncompressed BMP"));
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let pitch = (3 * w + 3) & !3;
    let bits = bytes
        .get(bits_at..)
        .and_then(|b| b.get(..pitch * h))
        .ok_or_else(|| invalid("truncated pixels"))?;
    let mut pixels = Vec::with_capacity(w * h);
    for y in 0..h {
        // A positive height stores the rows upside down.
        let row = if height > 0 { h - 1 - y } else { y };
        let row = &bits[row * pitch..][..3 * w];
        pixels.extend(row.as_chunks::<3>().0.iter().map(|p| {
            let [b, g, r] = p.map(u32::from);
            0xff00_0000 | (r << 16) | (g << 8) | b
        }));
    }
    Ok(Image {
        size: Size::from((w as i32, h as i32)),
        pixels,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 24-bit BMP of `w`×`h` whose pixel at (x, y) is `pixel(x, y)` as 0xRRGGBB, its rows
    /// stored bottom-up, or top-down with `top_down`.
    pub fn bmp(w: usize, h: usize, top_down: bool, pixel: impl Fn(usize, usize) -> u32) -> Vec<u8> {
        let pitch = (3 * w + 3) & !3;
        let mut bytes = Vec::new();
        bytes.extend(b"BM");
        bytes.extend(((54 + pitch * h) as u32).to_le_bytes());
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(54u32.to_le_bytes());
        bytes.extend(40u32.to_le_bytes());
        bytes.extend((w as i32).to_le_bytes());
        let height = if top_down { -(h as i32) } else { h as i32 };
        bytes.extend(height.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(24u16.to_le_bytes());
        bytes.extend([0; 24]);
        for row in 0..h {
            let y = if top_down { row } else { h - 1 - row };
            for x in 0..w {
                let p = pixel(x, y);
                bytes.extend([p as u8, (p >> 8) as u8, (p >> 16) as u8]);
            }
            bytes.extend(vec![0; pitch - 3 * w]);
        }
        bytes
    }

    fn pattern(x: usize, y: usize) -> u32 {
        ((x as u32) << 16) | ((y as u32) << 8) | 0x42
    }

    #[test]
    fn rows_come_out_top_first_either_way() {
        // 5 wide: three bytes of padding per row.
        for top_down in [false, true] {
            let image = decode_bmp(&bmp(5, 3, top_down, pattern)).unwrap();
            assert_eq!(image.size, Size::from((5, 3)));
            for y in 0..3 {
                for x in 0..5 {
                    assert_eq!(
                        image.pixels[y * 5 + x],
                        0xff00_0000 | pattern(x, y),
                        "({x}, {y}), top_down {top_down}"
                    );
                }
            }
        }
    }

    #[test]
    fn what_plymouth_refuses_is_refused() {
        let good = bmp(4, 4, false, pattern);
        assert!(decode_bmp(&good).is_ok());
        let mut thirty_two = good.clone();
        thirty_two[28] = 32;
        assert!(decode_bmp(&thirty_two).is_err());
        let mut compressed = good.clone();
        compressed[30] = 1;
        assert!(decode_bmp(&compressed).is_err());
        assert!(decode_bmp(&good[..good.len() - 1]).is_err());
        assert!(decode_bmp(&good[..20]).is_err());
        assert!(decode_bmp(b"\x89PNG").is_err());
    }

    /// A table as the kernel exports it, in a fresh directory.
    pub fn table(name: &str, status: u32, offset: (i32, i32), image: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("synoik-bgrt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("status"), format!("{status}\n")).unwrap();
        fs::write(dir.join("xoffset"), format!("{}\n", offset.0)).unwrap();
        fs::write(dir.join("yoffset"), format!("{}\n", offset.1)).unwrap();
        fs::write(dir.join("image"), image).unwrap();
        dir
    }

    #[test]
    fn the_table_gives_the_logo_and_its_place() {
        let dir = table("load", 1, (512, 272), &bmp(4, 2, false, pattern));
        let logo = Logo::load(&dir).unwrap();
        assert_eq!(logo.offset, Point::from((512, 272)));
        assert_eq!(logo.image.size, Size::from((4, 2)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_logo_for_a_rotated_panel_is_left_out() {
        let dir = table("rotated", 1 | (1 << 1), (0, 0), &bmp(4, 2, false, pattern));
        assert_eq!(Logo::load(&dir), None);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_table_no_logo() {
        assert_eq!(Logo::load(Path::new("/nonexistent/bgrt")), None);
    }
}
