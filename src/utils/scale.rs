// SPDX-License-Identifier: GPL-3.0-or-later
//
// From niri, copyright Ivan Molodetskikh and the niri contributors.

//! Default monitor scale calculation, and the ladder of scales a mode supports.
//!
//! Ported from mutter 50.3 `src/backends/meta-monitor.c` — `calculate_scale` (the DPI heuristic)
//! and `meta_monitor_calculate_supported_scales` / `for_each_scale` (the ladder). The ladder is
//! **not** a plain quarter-step sequence: a scale is only offered when it divides the mode into an
//! integer logical size, which is why GNOME shows 133% on a 2560x1440 panel where a quarter-step
//! ladder would show 125%/150%. Both compositors must agree here or the same display gets a
//! different set of choices in Settings and a different computed default.

use smithay::utils::{Physical, Raw, Size};

/// Bounds on the whole-number part of a scale (mutter's `MIN_/MAX_INTEGER_SCALE`).
const MIN_INTEGER_SCALE: u32 = 1;
const MAX_INTEGER_SCALE: u32 = 4;
/// Largest denominator a fractional scale may have — 4, so quarters at most (`MAX_DENOMINATOR`).
const MAX_DENOMINATOR: u32 = 4;
/// A scale that leaves less logical area than this is not offered (`MINIMUM_LOGICAL_AREA`).
const MIN_LOGICAL_AREA: i64 = 600 * 600;

const MOBILE_TARGET_DPI: f64 = 135.;
const LARGE_TARGET_DPI: f64 = 110.;
const LARGE_MIN_SIZE_INCHES: f64 = 20.;

/// Calculates the ideal scale for a monitor.
pub fn guess_monitor_scale(size_mm: Size<i32, Raw>, resolution: Size<i32, Physical>) -> f64 {
    // Somebody encoded the aspect ratio instead of the physical size; no scale can be derived from
    // it (`meta_monitor_has_aspect_as_size`, meta-monitor-manager.c:1597).
    if has_aspect_as_size(size_mm) {
        return 1.;
    }
    if size_mm.w == 0 || size_mm.h == 0 {
        return 1.;
    }

    let diag_inches = f64::from(size_mm.w * size_mm.w + size_mm.h * size_mm.h).sqrt() / 25.4;

    let target_dpi = if diag_inches < LARGE_MIN_SIZE_INCHES {
        MOBILE_TARGET_DPI
    } else {
        LARGE_TARGET_DPI
    };

    let physical_dpi =
        f64::from(resolution.w * resolution.w + resolution.h * resolution.h).sqrt() / diag_inches;
    let perfect_scale = physical_dpi / target_dpi;

    supported_scales(resolution)
        .map(|scale| (scale, (scale - perfect_scale).abs()))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map_or(1., |(scale, _)| scale)
}

/// Every scale this mode supports, ascending — mutter's `meta_monitor_calculate_supported_scales`.
///
/// A scale qualifies only when it divides the resolution into an *integer* logical size, which is
/// what keeps the compositor off fractional logical geometry; expressed as `numerator/denominator`
/// (denominator ≤ 4), that is `width * denominator % numerator == 0` for both axes. Equivalent
/// fractions are dropped, and a scale that would leave too little logical area is rejected. Falls
/// back to `[1.0]` when nothing qualifies, as mutter does.
pub fn supported_scales(resolution: Size<i32, Physical>) -> impl Iterator<Item = f64> {
    let mut scales = vec![];
    let (Ok(width), Ok(height)) = (u32::try_from(resolution.w), u32::try_from(resolution.h)) else {
        return vec![1.].into_iter();
    };

    for denominator in 1..=MAX_DENOMINATOR {
        for numerator in MIN_INTEGER_SCALE * denominator..=MAX_INTEGER_SCALE * denominator {
            // Accept only scales that divide perfectly into the screen.
            if (width * denominator) % numerator != 0 || (height * denominator) % numerator != 0 {
                continue;
            }
            // Eliminate equivalent fractions (duplicate scales).
            if highest_common_factor(numerator, denominator) > 1 {
                continue;
            }
            let scale = f64::from(numerator) / f64::from(denominator);
            if !is_valid_for_resolution(resolution, scale) {
                continue;
            }
            scales.push(scale);
        }
    }

    scales.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
    if scales.is_empty() {
        scales.push(1.);
    }
    scales.into_iter()
}

fn highest_common_factor(mut a: u32, mut b: u32) -> u32 {
    while a > 0 && b > 0 {
        if b > a {
            b %= a;
        } else {
            a %= b;
        }
    }
    a.max(b)
}

/// mutter's `is_scale_valid_for_size`: within the integer bounds, and the logical size (floored,
/// not rounded) must still be a usable desktop.
fn is_valid_for_resolution(resolution: Size<i32, Physical>, scale: f64) -> bool {
    if scale < f64::from(MIN_INTEGER_SCALE) || scale > f64::from(MAX_INTEGER_SCALE) {
        return false;
    }
    let logical = resolution.to_f64().to_logical(scale);
    i64::from(logical.w.floor() as i32) * i64::from(logical.h.floor() as i32) >= MIN_LOGICAL_AREA
}

/// Physical sizes that are really an aspect ratio (`meta_monitor_has_aspect_as_size`).
fn has_aspect_as_size(size_mm: Size<i32, Raw>) -> bool {
    matches!(
        (size_mm.w, size_mm.h),
        (1600, 900) | (1600, 1000) | (160, 90) | (160, 100) | (16, 9) | (16, 10)
    )
}

/// Adjusts the scale to the closest exactly-representable value.
pub fn closest_representable_scale(scale: f64) -> f64 {
    // Current fractional-scale Wayland protocol can only represent N / 120 scales.
    const FRACTIONAL_SCALE_DENOM: f64 = 120.;

    (scale * FRACTIONAL_SCALE_DENOM).round() / FRACTIONAL_SCALE_DENOM
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    fn check(size_mm: (i32, i32), resolution: (i32, i32)) -> f64 {
        guess_monitor_scale(Size::from(size_mm), Size::from(resolution))
    }

    #[test]
    fn test_guess_monitor_scale() {
        // Librem 5; 2.0 leaves too little logical area, and 1.75 is not on this mode's ladder
        assert_snapshot!(check((65, 129), (720, 1440)), @"1.6666666666666667");
        // OnePlus 6
        assert_snapshot!(check((68, 144), (1080, 2280)), @"2.5");
        // Google Pixel 6a
        assert_snapshot!(check((64, 142), (1080, 2400)), @"2.6666666666666665");
        // 13" MacBook Retina; 1.75 does not divide 2560x1600 into an integer logical size
        assert_snapshot!(check((286, 179), (2560, 1600)), @"1.6666666666666667");
        // Surface Laptop Studio
        assert_snapshot!(check((303, 202), (2400, 1600)), @"1.3333333333333333");
        // Dell XPS 9320
        assert_snapshot!(check((290, 180), (3840, 2400)), @"2.5");
        // Lenovo ThinkPad X1 Yoga Gen 6
        assert_snapshot!(check((300, 190), (3840, 2400)), @"2.5");
        // Generic 23" 1080p
        assert_snapshot!(check((509, 286), (1920, 1080)), @"1");
        // Generic 23" 4K
        assert_snapshot!(check((509, 286), (3840, 2160)), @"1.6666666666666667");
        // Generic 27" 4K
        assert_snapshot!(check((598, 336), (3840, 2160)), @"1.5");
        // Generic 32" 4K
        assert_snapshot!(check((708, 398), (3840, 2160)), @"1.25");
        // Generic 25" 4K; ideal scale is 1.60, and 1.667 is nearer than 1.5
        assert_snapshot!(check((554, 312), (3840, 2160)), @"1.6666666666666667");
        // Generic 23.5" 4K; ideal scale is 1.70
        assert_snapshot!(check((522, 294), (3840, 2160)), @"1.6666666666666667");
        // Lenovo Legion 7 Gen 7 AMD 16"
        assert_snapshot!(check((340, 210), (2560, 1600)), @"1.3333333333333333");
        // Acer Nitro XV320QU LV 31.5"
        assert_snapshot!(check((700, 390), (2560, 1440)), @"1");
        // Surface Pro 6
        assert_snapshot!(check((260, 170), (2736, 1824)), @"2");
    }

    #[test]
    fn ladder_only_offers_integer_logical_sizes() {
        let ladder = |w, h| {
            supported_scales(Size::from((w, h)))
                .map(|s| format!("{s:.3}"))
                .collect::<Vec<_>>()
                .join(" ")
        };

        // The list mutter offers for this mode, quoted in the limina report — 1.75 is absent
        // because 2560 * 4 is not divisible by 7, and 1.5 because 1440 / 1.5 = 960 but
        // 2560 / 1.5 is not an integer.
        assert_snapshot!(
            ladder(2560, 1440),
            @"1.000 1.250 1.333 1.667 2.000 2.500 2.667"
        );
        // 4K divides by every quarter step, so here the ladder does look quarter-ish — but it
        // still carries the thirds mutter offers.
        assert_snapshot!(
            ladder(3840, 2160),
            @"1.000 1.250 1.333 1.500 1.667 2.000 2.500 2.667 3.000 3.333 3.750 4.000"
        );
        // A mode too small to divide at all falls back to 1.0 rather than offering nothing.
        assert_snapshot!(ladder(640, 480), @"1.000");
    }

    #[test]
    fn guess_monitor_scale_unknown_size() {
        assert_eq!(check((0, 0), (1920, 1080)), 1.);
    }

    #[test]
    fn test_round_scale() {
        assert_snapshot!(closest_representable_scale(1.3), @"1.3");
        assert_snapshot!(closest_representable_scale(1.31), @"1.3083333333333333");
        assert_snapshot!(closest_representable_scale(1.32), @"1.3166666666666667");
        assert_snapshot!(closest_representable_scale(1.33), @"1.3333333333333333");
        assert_snapshot!(closest_representable_scale(1.34), @"1.3416666666666666");
        assert_snapshot!(closest_representable_scale(1.35), @"1.35");
    }
}
