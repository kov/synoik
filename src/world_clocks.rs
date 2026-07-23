//! World-clocks data plane for the dateMenu popover.
//!
//! A fork-owned port of gnome-shell's `WorldClocksSection` (`js/ui/dateMenu.js:331-541`):
//! the `%card` below the Events section listing the user's configured city clocks
//! (city name | current time there | UTC offset relative to local). The list is stored
//! in the `org.gnome.shell.world-clocks` GSettings key `locations` (`av`), each element a
//! GWeather-serialized location `(uv)` = `(format_version, (name, station, valid, coords,
//! coords2))`. Crucially the **timezone is not in the blob** — GWeather recovers it from its
//! embedded location DB. Per the fork's GObject-free direction we resolve the timezone from
//! the serialized **coordinates** instead (`tzf-rs`, a pure-Rust coordinate→IANA-tz lookup),
//! then compute wall time / offsets with `jiff` (system `zoneinfo`, so tz-rule updates apply
//! without a rebuild) + libc `strftime` (locale-correct AM/PM, byte-identical to the panel
//! clock and events labels).
//!
//! ## Memory
//! `tzf-rs`'s `DefaultFinder` embeds ~58 MiB (peak) of boundary polygons. It is therefore
//! built **transiently** in [`resolve_timezones`] only when the locations blob is non-empty
//! (the common case — no world clocks — never touches it), the resolved IANA ids are cached,
//! and the finder is dropped. Steady-state residency is ~0.
//!
//! ## Divergences from gnome-shell (recorded)
//! - Timezone from coordinates (tzf-rs), not GWeather's DB lookup — a border approximation,
//!   immaterial for cities.
//! - City labels are the serialized (English) name; GNOME shows `get_city_name() || get_name()` =
//!   the localized DB name after lookup, so non-English locales differ.
//! - Clicking the section is wired to `gtk-launch org.gnome.clocks` at the UI layer, not a real
//!   app-activate (the repo-wide app-activation divergence).
//! - RTL column order is the repo-wide deferred-RTL divergence.

use std::ffi::CStr;

use gio::glib;

/// The GWeather location serialization format we understand (`dateMenu.js` stores v2).
const LOCATIONS_FORMAT_VERSION: u32 = 2;

/// A configured location parsed out of the GSettings blob, before timezone resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldLocation {
    /// The serialized location name (English; see the module divergence note).
    pub name: String,
    /// Latitude in degrees (the blob stores radians).
    pub lat_deg: f64,
    /// Longitude in degrees.
    pub lon_deg: f64,
}

/// A location whose IANA timezone has been resolved from its coordinates. This is the
/// cacheable output of the expensive [`resolve_timezones`] step (keyed by the blob).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLocation {
    pub name: String,
    /// IANA timezone id, e.g. `"Europe/London"`.
    pub tz_id: String,
}

/// One rendered clock row (`.world-clocks-city` | `.world-clocks-time` | `.world-clocks-timezone`).
#[derive(Debug, Clone, PartialEq)]
pub struct ClockRow {
    pub city: String,
    /// Current wall-clock time there, formatted like the panel clock (`3:04 PM` / `15:04`).
    pub time: String,
    /// UTC offset **relative to local**, `±H` or `±H:MM` (`_getTimezoneOffsetAtLocation`).
    pub tz_offset: String,
}

/// The model handed to the UI card (`_clocksChanged` + the per-minute label refresh).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorldClocksModel {
    /// The whole section is shown iff GNOME Clocks is installed (`_sync`, `dateMenu.js:384-387`).
    pub visible: bool,
    /// `"World Clocks"`, or `"Add World Clocks…"` when there are no resolvable rows.
    pub header: String,
    /// True when there are no rows — the header then draws in `$fg_color` (`.no-world-clocks`)
    /// instead of the muted `$card_insensitive_fg_color`.
    pub empty: bool,
    /// Clocks sorted ascending by current UTC offset (stable — ties keep settings order).
    pub rows: Vec<ClockRow>,
}

/// Parse the `org.gnome.shell.world-clocks locations` (`av`) blob into locations.
///
/// Each element is a `v`-boxed `(uv)` whose inner `v` boxes `(ssba(dd)a(dd))`. Entries with a
/// non-v2 format version, or with an empty coordinate list, are skipped (the analog of GNOME's
/// `deserialize() && get_timezone() != null` filter, `dateMenu.js:395-399`).
pub fn parse_locations(value: &glib::Variant) -> Vec<WorldLocation> {
    (0..value.n_children())
        .filter_map(|i| parse_location(&value.child_value(i)))
        .collect()
}

/// Parse one `av` element (a `v` boxing `(uv)`).
fn parse_location(element: &glib::Variant) -> Option<WorldLocation> {
    // The `av` element is variant-typed: unbox to the `(uv)` tuple.
    let entry = element.as_variant()?;
    let version = entry.child_value(0).get::<u32>()?;
    if version != LOCATIONS_FORMAT_VERSION {
        return None;
    }
    // Field 1 is again variant-typed: unbox to `(ssba(dd)a(dd))`.
    let inner = entry.child_value(1).as_variant()?;
    let (name, _station, _valid, coords, _coords2) =
        inner.get::<(String, String, bool, Vec<(f64, f64)>, Vec<(f64, f64)>)>()?;
    // The location's own coordinates are the first `(lat, lon)` pair, in radians. An empty
    // list means GWeather couldn't place it → skip, like a null timezone upstream.
    let (lat_rad, lon_rad) = *coords.first()?;
    Some(WorldLocation {
        name,
        lat_deg: lat_rad.to_degrees(),
        lon_deg: lon_rad.to_degrees(),
    })
}

/// Resolve each location's coordinates to an IANA timezone id.
///
/// Builds a `tzf-rs` finder transiently (see the module Memory note) and drops it on return.
/// Meant to run off the frame loop (a worker thread) because the finder's construction is
/// expensive. Returns an entry per input location, in input order.
pub fn resolve_timezones(locations: &[WorldLocation]) -> Vec<ResolvedLocation> {
    if locations.is_empty() {
        return Vec::new();
    }
    let finder = tzf_rs::DefaultFinder::new();
    locations
        .iter()
        .map(|l| ResolvedLocation {
            name: l.name.clone(),
            // tzf-rs takes (lng, lat) in degrees.
            tz_id: finder.get_tz_name(l.lon_deg, l.lat_deg).to_string(),
        })
        .collect()
    // `finder` (and its ~58 MiB) is dropped here.
}

/// Build the render model from resolved locations at a given instant.
///
/// Rows are formatted (time + offset) and sorted by current UTC offset. `clocks_installed`
/// gates section visibility; `now_secs` is the (pinned, in tests) wall-clock; `is_24h` follows
/// `org.gnome.desktop.interface clock-format`.
pub fn world_clocks_model(
    resolved: &[ResolvedLocation],
    clocks_installed: bool,
    now_secs: i64,
    is_24h: bool,
) -> WorldClocksModel {
    let local_off = local_offset_secs(now_secs);
    let mut rows: Vec<(i32, ClockRow)> = resolved
        .iter()
        .filter_map(|r| {
            // A tz id tzf produced but the system tzdb lacks → skip the row (GNOME's null-tz
            // filter). `jiff` reads the system zoneinfo, so this is the tzdb-skew guard.
            let tz = jiff::tz::TimeZone::get(&r.tz_id).ok()?;
            let ts = jiff::Timestamp::from_second(now_secs).ok()?;
            let off = tz.to_offset(ts).seconds();
            Some((
                off,
                ClockRow {
                    city: r.name.clone(),
                    time: format_time_at_offset(now_secs, off, is_24h),
                    tz_offset: format_offset_delta(off - local_off),
                },
            ))
        })
        .collect();
    // Ascending by offset; stable so equal-offset cities keep their settings order.
    rows.sort_by_key(|(off, _)| *off);

    let rows: Vec<ClockRow> = rows.into_iter().map(|(_, r)| r).collect();
    let empty = rows.is_empty();
    WorldClocksModel {
        visible: clocks_installed,
        header: if empty {
            "Add World Clocks…"
        } else {
            "World Clocks"
        }
        .to_string(),
        empty,
        rows,
    }
}

/// The system's current UTC offset in seconds, via the same tz source as the city side
/// (`jiff` system zone) so the two never disagree (`_getTimezoneOffsetAtLocation` uses GLib
/// for both).
fn local_offset_secs(now_secs: i64) -> i32 {
    let Ok(ts) = jiff::Timestamp::from_second(now_secs) else {
        return 0;
    };
    jiff::tz::TimeZone::system().to_offset(ts).seconds()
}

/// Current wall-clock time at a UTC offset, formatted like the panel clock.
///
/// `gmtime(now + offset)` decomposes to the target zone's wall clock (offset already applied),
/// so no per-zone `localtime`/TZ swap is needed; `strftime`'s `%p` stays locale-correct.
fn format_time_at_offset(now_secs: i64, off_secs: i32, is_24h: bool) -> String {
    let wall = now_secs + i64::from(off_secs);
    // SAFETY: gmtime_r writes into the provided tm; no static buffer aliasing.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = wall as libc::time_t;
    unsafe {
        libc::gmtime_r(&t, &mut tm);
    }
    strftime_tm(&tm, if is_24h { c"%H:%M" } else { c"%-I:%M %p" })
}

fn strftime_tm(tm: &libc::tm, fmt: &CStr) -> String {
    let mut buf = [0u8; 64];
    // SAFETY: buf/len/fmt/tm are all valid; strftime writes at most len bytes.
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            tm,
        )
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Format a location's offset relative to local (`_getTimezoneOffsetAtLocation`,
/// `dateMenu.js:490-506`): `±H`, or `±H:MM` when the zone has sub-hour minutes (only :30/:45
/// occur). Same-zone-as-local → `"+0"`.
fn format_offset_delta(delta_secs: i32) -> String {
    let sign = if delta_secs >= 0 { '+' } else { '-' };
    let abs = delta_secs.unsigned_abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    if minutes == 0 {
        format!("{sign}{hours}")
    } else {
        // GNOME prints the minutes un-padded; only :30/:45 ever occur so this reads naturally.
        format!("{sign}{hours}:{minutes}")
    }
}

#[cfg(test)]
mod tests {
    use gio::prelude::ToVariant;

    use super::*;

    /// Build one serialized location `(uv)` = `(2, (name, station, false, [(lat, lon)], []))`,
    /// the exact shape GWeather stores in the GSettings blob (verified via GObject
    /// introspection this session). `(u32, Variant)::to_variant()` boxes the inner tuple into
    /// the `v` field, giving `(uv)`.
    fn loc_uv(name: &str, station: &str, lat_rad: f64, lon_rad: f64) -> glib::Variant {
        let inner = (
            name.to_string(),
            station.to_string(),
            false,
            vec![(lat_rad, lon_rad)],
            Vec::<(f64, f64)>::new(),
        )
            .to_variant();
        (2u32, inner).to_variant()
    }

    /// Wrap `(uv)` entries into an `av` of `v`-boxes, as the `locations` key stores them.
    fn av_of(entries: impl IntoIterator<Item = glib::Variant>) -> glib::Variant {
        glib::Variant::array_from_iter_with_type(
            glib::VariantTy::VARIANT,
            entries.into_iter().map(|e| glib::Variant::from_variant(&e)),
        )
    }

    /// London (Heathrow, EGLL) + Tokyo (Haneda, RJTT) with the real serialized radian coords.
    fn real_av() -> glib::Variant {
        av_of([
            loc_uv(
                "Heathrow Airport",
                "EGLL",
                0.898_553_670_750_65,
                -0.007_853_981_633_974,
            ),
            loc_uv(
                "Tokyo International Airport",
                "RJTT",
                0.620_464_549_083_98,
                2.439_679_400_261_64,
            ),
        ])
    }

    #[test]
    fn parses_serialized_locations() {
        let locs = parse_locations(&real_av());
        assert_eq!(locs.len(), 2, "two v2 entries");
        assert_eq!(locs[0].name, "Heathrow Airport");
        assert_eq!(locs[1].name, "Tokyo International Airport");
        // Heathrow ≈ 51.48°N, -0.45°E; Haneda ≈ 35.55°N, 139.78°E.
        assert!(
            (locs[0].lat_deg - 51.48).abs() < 0.1,
            "lat {}",
            locs[0].lat_deg
        );
        assert!(
            (locs[0].lon_deg - -0.45).abs() < 0.1,
            "lon {}",
            locs[0].lon_deg
        );
        assert!(
            (locs[1].lat_deg - 35.55).abs() < 0.1,
            "lat {}",
            locs[1].lat_deg
        );
        assert!(
            (locs[1].lon_deg - 139.78).abs() < 0.1,
            "lon {}",
            locs[1].lon_deg
        );
    }

    #[test]
    fn skips_wrong_version_and_empty_coords() {
        // v1 entry (skipped), a v2 with empty coords (skipped), a good v2 (kept).
        let good = loc_uv("Reykjavik", "BIRK", 1.13, -0.38);
        let v1_inner = (
            "Old".to_string(),
            "XXXX".to_string(),
            false,
            vec![(1.0_f64, 1.0_f64)],
            Vec::<(f64, f64)>::new(),
        )
            .to_variant();
        let v1 = (1u32, v1_inner).to_variant();
        let empty_inner = (
            "Nowhere".to_string(),
            "ZZZZ".to_string(),
            false,
            Vec::<(f64, f64)>::new(),
            Vec::<(f64, f64)>::new(),
        )
            .to_variant();
        let v2_empty = (2u32, empty_inner).to_variant();
        let av = av_of([v1, v2_empty, good]);
        let locs = parse_locations(&av);
        assert_eq!(locs.len(), 1);
        assert_eq!(locs[0].name, "Reykjavik");
    }

    #[test]
    fn resolves_coordinates_to_timezones() {
        // The one test that pays tzf-rs's construction cost; also pins the (lng, lat) order —
        // a swapped call would land these coords in the wrong hemisphere / ocean.
        let locs = parse_locations(&real_av());
        let resolved = resolve_timezones(&locs);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].tz_id, "Europe/London");
        assert_eq!(resolved[1].tz_id, "Asia/Tokyo");
    }

    #[test]
    fn offset_delta_formatting() {
        // Relative to local: hours only, sub-hour minutes, sign, and same-zone zero.
        assert_eq!(format_offset_delta(9 * 3600), "+9");
        assert_eq!(format_offset_delta(-5 * 3600), "-5");
        assert_eq!(format_offset_delta(0), "+0");
        assert_eq!(format_offset_delta(5 * 3600 + 30 * 60), "+5:30");
        assert_eq!(format_offset_delta(-(3 * 3600 + 30 * 60)), "-3:30");
    }

    #[test]
    fn time_at_offset_applies_the_zone() {
        // 2021-01-01 00:00:00 UTC. At +9h it is 09:00 the same day; at -5h it is the previous
        // day 19:00. gmtime(now+off) must reflect the shifted wall clock.
        let utc_midnight = 1_609_459_200_i64;
        assert_eq!(format_time_at_offset(utc_midnight, 9 * 3600, true), "09:00");
        assert_eq!(
            format_time_at_offset(utc_midnight, -5 * 3600, true),
            "19:00"
        );
        // 12h format keeps locale AM/PM.
        assert_eq!(
            format_time_at_offset(utc_midnight, 9 * 3600, false),
            "9:00 AM"
        );
    }

    #[test]
    fn model_sorts_by_offset_and_sets_header() {
        // Two hand-made resolved zones; UTC-negative sorts before UTC-positive.
        let resolved = vec![
            ResolvedLocation {
                name: "Tokyo".into(),
                tz_id: "Asia/Tokyo".into(),
            },
            ResolvedLocation {
                name: "New York".into(),
                tz_id: "America/New_York".into(),
            },
        ];
        let m = world_clocks_model(&resolved, true, 1_609_459_200, true);
        assert!(m.visible);
        assert!(!m.empty);
        assert_eq!(m.header, "World Clocks");
        assert_eq!(m.rows.len(), 2);
        assert_eq!(m.rows[0].city, "New York", "west-of-UTC sorts first");
        assert_eq!(m.rows[1].city, "Tokyo");

        // Empty → the add-clocks header + fg color flag.
        let empty = world_clocks_model(&[], true, 1_609_459_200, true);
        assert!(empty.empty);
        assert_eq!(empty.header, "Add World Clocks…");
        // Not installed → whole section hidden.
        assert!(!world_clocks_model(&resolved, false, 1_609_459_200, true).visible);
    }
}
