// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cmp::{max, min};
use std::ffi::{CString, OsStr};
use std::fmt::Display;
use std::io::Write;
use std::os::unix::prelude::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use std::{f64, fmt};

use anyhow::{ensure, Context};
use bitflags::bitflags;
use directories::UserDirs;
use git_version::git_version;
use smithay::backend::renderer::utils::{
    with_renderer_surface_state, RendererSurfaceStateUserData,
};
use smithay::input::pointer::CursorIcon;
use smithay::output::{self, Output};
use smithay::reexports::rustix::time::{clock_gettime, ClockId};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource as _};
use smithay::utils::{Coordinate, Logical, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::compositor::{send_surface_state, with_states, SurfaceData};
use smithay::wayland::fractional_scale::with_fractional_scale;
use smithay::wayland::shell::xdg::{
    ToplevelCachedState, ToplevelConfigure, ToplevelState, ToplevelSurface, XdgToplevelSurfaceData,
    XdgToplevelSurfaceRoleAttributes,
};
use synoik_config::{Config, OutputName};
use wayland_backend::server::Credentials;

use crate::handlers::KdeDecorationsModeState;
use crate::synoik::ClientState;

pub mod id;
pub mod region;
pub mod scale;
pub mod signals;
pub mod spawning;
pub mod transaction;
pub mod vblank_throttle;
pub mod xwayland;

pub static IS_SYSTEMD_SERVICE: AtomicBool = AtomicBool::new(false);

use id::IdCounter;

/// Unique ID for a screencast session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CastSessionId(u64);

impl CastSessionId {
    pub fn next() -> Self {
        static COUNTER: IdCounter = IdCounter::new();
        Self(COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl Display for CastSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u64> for CastSessionId {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

/// Unique ID for a screencast stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CastStreamId(u64);

impl CastStreamId {
    pub fn next() -> Self {
        static COUNTER: IdCounter = IdCounter::new();
        Self(COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl Display for CastStreamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;

        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();

        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();

        const LEFT_RIGHT   = Self::LEFT.bits() | Self::RIGHT.bits();
        const TOP_BOTTOM   = Self::TOP.bits() | Self::BOTTOM.bits();
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    #[inline]
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

impl ResizeEdge {
    pub fn cursor_icon(self) -> CursorIcon {
        match self {
            Self::LEFT => CursorIcon::WResize,
            Self::RIGHT => CursorIcon::EResize,
            Self::TOP => CursorIcon::NResize,
            Self::BOTTOM => CursorIcon::SResize,
            Self::TOP_LEFT => CursorIcon::NwResize,
            Self::TOP_RIGHT => CursorIcon::NeResize,
            Self::BOTTOM_RIGHT => CursorIcon::SeResize,
            Self::BOTTOM_LEFT => CursorIcon::SwResize,
            _ => CursorIcon::Default,
        }
    }
}

pub fn version() -> String {
    if let Some(v) = option_env!("SYNOIK_BUILD_VERSION_STRING") {
        return String::from(v);
    }

    const MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
    const MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
    const PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");

    let commit =
        option_env!("SYNOIK_BUILD_COMMIT").unwrap_or(git_version!(fallback = "unknown commit"));

    if PATCH == "0" {
        format!("{MAJOR}.{MINOR:0>2} ({commit})")
    } else {
        format!("{MAJOR}.{MINOR:0>2}.{PATCH} ({commit})")
    }
}

pub fn get_monotonic_time() -> Duration {
    let ts = clock_gettime(ClockId::Monotonic);
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

pub fn center(rect: Rectangle<i32, Logical>) -> Point<i32, Logical> {
    rect.loc + rect.size.downscale(2).to_point()
}

pub fn center_f64(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    rect.loc + rect.size.downscale(2.0).to_point()
}

/// Convert logical pixels to physical, rounding to physical pixels.
pub fn to_physical_precise_round<N: Coordinate>(scale: f64, logical: impl Coordinate) -> N {
    N::from_f64((logical.to_f64() * scale).round())
}

pub fn round_logical_in_physical(scale: f64, logical: f64) -> f64 {
    (logical * scale).round() / scale
}

pub fn round_logical_in_physical_max1(scale: f64, logical: f64) -> f64 {
    if logical == 0. {
        return 0.;
    }

    (logical * scale).max(1.).round() / scale
}

pub fn floor_logical_in_physical_max1(scale: f64, logical: f64) -> f64 {
    if logical == 0. {
        return 0.;
    }

    (logical * scale).max(1.).floor() / scale
}

pub fn output_size(output: &Output) -> Size<f64, Logical> {
    let output_scale = output.current_scale().fractional_scale();
    let output_transform = output.current_transform();
    let output_mode = output.current_mode().unwrap();
    let logical_size = output_mode.size.to_f64().to_logical(output_scale);
    output_transform.transform_size(logical_size)
}

pub fn logical_output(output: &Output, is_primary: bool) -> synoik_ipc::LogicalOutput {
    let loc = output.current_location();
    let size = output_size(output);
    let transform = match output.current_transform() {
        Transform::Normal => synoik_ipc::Transform::Normal,
        Transform::_90 => synoik_ipc::Transform::_90,
        Transform::_180 => synoik_ipc::Transform::_180,
        Transform::_270 => synoik_ipc::Transform::_270,
        Transform::Flipped => synoik_ipc::Transform::Flipped,
        Transform::Flipped90 => synoik_ipc::Transform::Flipped90,
        Transform::Flipped180 => synoik_ipc::Transform::Flipped180,
        Transform::Flipped270 => synoik_ipc::Transform::Flipped270,
    };
    synoik_ipc::LogicalOutput {
        x: loc.x,
        y: loc.y,
        width: size.w as u32,
        height: size.h as u32,
        scale: output.current_scale().fractional_scale(),
        transform,
        is_primary,
    }
}

pub struct PanelOrientation(pub Transform);
pub fn panel_orientation(output: &Output) -> Transform {
    output
        .user_data()
        .get::<PanelOrientation>()
        .map(|x| x.0)
        .unwrap_or(Transform::Normal)
}

pub fn ipc_transform_to_smithay(transform: synoik_ipc::Transform) -> Transform {
    match transform {
        synoik_ipc::Transform::Normal => Transform::Normal,
        synoik_ipc::Transform::_90 => Transform::_90,
        synoik_ipc::Transform::_180 => Transform::_180,
        synoik_ipc::Transform::_270 => Transform::_270,
        synoik_ipc::Transform::Flipped => Transform::Flipped,
        synoik_ipc::Transform::Flipped90 => Transform::Flipped90,
        synoik_ipc::Transform::Flipped180 => Transform::Flipped180,
        synoik_ipc::Transform::Flipped270 => Transform::Flipped270,
    }
}

pub fn is_mapped(surface: &WlSurface) -> bool {
    // None if the surface hadn't committed yet.
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

pub fn send_scale_transform(
    surface: &WlSurface,
    data: &SurfaceData,
    scale: output::Scale,
    transform: Transform,
) {
    send_surface_state(surface, data, scale.integer_scale(), transform);
    with_fractional_scale(data, |fractional| {
        fractional.set_preferred_scale(scale.fractional_scale());
    });
}

pub fn expand_home(path: &Path) -> anyhow::Result<Option<PathBuf>> {
    if let Ok(rest) = path.strip_prefix("~") {
        let dirs = UserDirs::new().context("error retrieving home directory")?;
        Ok(Some([dirs.home_dir(), rest].iter().collect()))
    } else {
        Ok(None)
    }
}

pub fn make_screenshot_path(config: &Config) -> anyhow::Result<Option<PathBuf>> {
    let Some(path) = &config.screenshot_path.0 else {
        return Ok(None);
    };

    let format = CString::new(path.clone()).context("path must not contain nul bytes")?;

    let mut buf = [0u8; 2048];
    let mut path;
    unsafe {
        let time = libc::time(null_mut());
        ensure!(time != -1, "error in time()");

        let tm = libc::localtime(&time);
        ensure!(!tm.is_null(), "error in localtime()");

        let rv = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), tm);
        ensure!(rv != 0, "error formatting time");

        path = PathBuf::from(OsStr::from_bytes(&buf[..rv]));
    }

    if let Some(expanded) = expand_home(&path).context("error expanding ~")? {
        path = expanded;
    }

    Ok(Some(path))
}

pub fn write_png_rgba8(
    w: impl Write,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)
}

/// Crop a tightly-packed RGBA8 buffer to `area`, clamped to what the buffer actually holds.
///
/// Clamping rather than failing is what `ScreenshotArea` needs: a caller is free to ask for a
/// rectangle that runs off the edge of the screen, and GNOME hands back the part that exists.
/// An area that misses the buffer entirely is an error, because there is no image to return.
pub fn crop_rgba8(
    size: Size<i32, Physical>,
    pixels: &[u8],
    area: Rectangle<i32, Physical>,
) -> anyhow::Result<(Size<i32, Physical>, Vec<u8>)> {
    let bounds = Rectangle::from_size(size);
    let area = area
        .intersection(bounds)
        .filter(|a| a.size.w > 0 && a.size.h > 0)
        .context("the requested area is not on screen")?;

    if area == bounds {
        return Ok((size, pixels.to_vec()));
    }

    let stride = size.w as usize * 4;
    let row = area.size.w as usize * 4;
    let x = area.loc.x as usize * 4;
    let mut out = Vec::with_capacity(row * area.size.h as usize);
    for y in 0..area.size.h as usize {
        let start = (area.loc.y as usize + y) * stride + x;
        out.extend_from_slice(&pixels[start..start + row]);
    }

    Ok((area.size, out))
}

pub fn output_matches_name(output: &Output, target: &str) -> bool {
    let name = output.user_data().get::<OutputName>().unwrap();
    name.matches(target)
}

/// Whether a connector drives a built-in panel: mutter's `meta_output_info_is_builtin`
/// (`meta-output.c:520-533`), the one predicate behind both the "Built-in display" name and the
/// backlight device preference ([`crate::backlight::find_backlight`]).
///
/// The prefixes are the drm crate's `Interface::as_str` spellings, which are the kernel's.
pub fn is_laptop_panel(connector: &str) -> bool {
    ["eDP-", "LVDS-", "DSI-", "DPI-"]
        .iter()
        .any(|prefix| connector.starts_with(prefix))
}

/// The monitor's user-visible name — mutter's `meta_monitor_get_display_name`.
///
/// Shared by the `DisplayConfig` service and the per-monitor brightness rows
/// ([`crate::backlight::OutputBacklight::display_name`]); gnome-shell names a brightness scale
/// after its first monitor's display name (`brightnessManager.js:338`).
// Adapted from Mutter.
pub fn make_display_name(output: &synoik_ipc::Output, is_laptop_panel: bool) -> String {
    if is_laptop_panel {
        return String::from("Built-in display");
    }

    let make = &output.make;
    let model = &output.model;
    if let Some(diagonal) = output.physical_size.map(|(width_mm, height_mm)| {
        let diagonal = f64::hypot(f64::from(width_mm), f64::from(height_mm)) / 25.4;
        format_diagonal(diagonal)
    }) {
        format!("{make} {diagonal}")
    } else if model != "Unknown" {
        format!("{make} {model}")
    } else {
        make.clone()
    }
}

pub fn format_diagonal(diagonal_inches: f64) -> String {
    let known = [12.1, 13.3, 15.6];
    if let Some(d) = known.iter().find(|d| (*d - diagonal_inches).abs() < 0.1) {
        format!("{d:.1}″")
    } else {
        format!("{}″", diagonal_inches.round() as u32)
    }
}

/// Returns the geometry of the surface.
///
/// Returns `None` if the surface isn't mapped.
pub fn surface_geo(states: &SurfaceData) -> Option<Rectangle<i32, Logical>> {
    let data = states.data_map.get::<RendererSurfaceStateUserData>();
    data.and_then(|d| d.lock().unwrap().view())
        .map(|view| Rectangle {
            loc: view.offset,
            size: view.dst,
        })
}

pub fn with_toplevel_role<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(&mut XdgToplevelSurfaceRoleAttributes) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        let mut role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();

        f(&mut role)
    })
}

pub fn with_toplevel_role_and_current<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(&mut XdgToplevelSurfaceRoleAttributes, Option<&ToplevelState>) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        let mut role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();

        let mut guard = states.cached_state.get::<ToplevelCachedState>();
        let current = guard.current().last_acked.as_ref().map(|c| &c.state);

        f(&mut role, current)
    })
}

pub fn with_toplevel_last_uncommitted_configure<T>(
    toplevel: &ToplevelSurface,
    f: impl FnOnce(Option<&ToplevelConfigure>) -> T,
) -> T {
    with_states(toplevel.wl_surface(), |states| {
        let role = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap();

        let mut guard = states.cached_state.get::<ToplevelCachedState>();

        if let Some(last_pending) = role.pending_configures().last() {
            // Configure not yet acked by the client.
            f(Some(last_pending))
        } else if let Some(last_acked) = &role.last_acked {
            let mut configure = Some(last_acked);

            if let Some(committed) = &guard.current().last_acked {
                if committed.serial.is_no_older_than(&last_acked.serial) {
                    // Already committed to this configure.
                    configure = None;
                }
            }

            f(configure)
        } else {
            // Surface hadn't been configured yet.
            f(None)
        }
    })
}

pub fn update_tiled_state(
    toplevel: &ToplevelSurface,
    prefer_no_csd: bool,
    force_tiled: Option<bool>,
) {
    // Determine the default value for our tiled state. The idea is to use the tiled state to
    // make windows rectangular even if they don't support xdg-decoration (e.g. GTK).
    //
    // If the user prefers no CSD, it's a reasonable assumption that they would prefer to get
    // rid of the various client-side rounded corners also by using the tiled state.
    let should_tile = || {
        // Figure out if the client bound any decoration globals for this window. In this case,
        // the pending decoration mode will be set to something (we always set it upon binding the
        // global and never reset to None).
        //
        // If the client bound a decoration global, use the mode that we negotiated. This way,
        // changing the decoration mode on the client at runtime will synchronize with the
        // default tiled state.
        if let Some(mode) = toplevel.with_pending_state(|state| state.decoration_mode) {
            mode == zxdg_toplevel_decoration_v1::Mode::ServerSide
        } else if let Some(mode) = with_states(toplevel.wl_surface(), |states| {
            states.data_map.get::<KdeDecorationsModeState>().cloned()
        }) {
            // Actually, make the KDE decoration overridable with prefer_no_csd. GTK 3 likes to
            // always request CSD through it, and we want prefer_no_csd to set the tiled state
            // automatically for GTK 3. Also, unlike xdg-decoration, KDE decoration is not
            // synchronized to commits, so that argument is less important.
            mode.is_server() || prefer_no_csd
        } else {
            // The client doesn't see or doesn't care about the decoration protocols. In this
            // case, use the current prefer_no_csd value as the user's intention.
            //
            // This is a bit weird because it makes it seem like prefer_no_csd can apply live,
            // while that isn't really the case. That's because prefer_no_csd controls two separate
            // things: whether the client sees the decoration globals, and the tiled state.
            //
            // A more accurate way would perhaps be to check if the client cannot see the
            // decoration globals, and in this case behave as if prefer_no_csd was false. However,
            // this also regresses the common case of GTK 4 applications that do not react to
            // xdg-decoration in any way, and therefore the tiled state *is* the "no CSD" mode from
            // the user's perspective, so by artificially gating it we would artificially make it
            // impossible to apply it live for GTK 4 applications.
            prefer_no_csd
        }
    };

    let should_tile = force_tiled.unwrap_or_else(should_tile);

    toplevel.with_pending_state(|state| {
        if should_tile {
            state.states.set(xdg_toplevel::State::TiledLeft);
            state.states.set(xdg_toplevel::State::TiledRight);
            state.states.set(xdg_toplevel::State::TiledTop);
            state.states.set(xdg_toplevel::State::TiledBottom);
        } else {
            state.states.unset(xdg_toplevel::State::TiledLeft);
            state.states.unset(xdg_toplevel::State::TiledRight);
            state.states.unset(xdg_toplevel::State::TiledTop);
            state.states.unset(xdg_toplevel::State::TiledBottom);
        }
    });
}

pub fn get_credentials_for_surface(surface: &WlSurface) -> Option<Credentials> {
    let handle = surface.handle().upgrade()?;
    let dh = DisplayHandle::from(handle);

    let client = dh.get_client(surface.id()).ok()?;
    get_credentials_for_client(&dh, &client)
}

pub fn get_credentials_for_client(dh: &DisplayHandle, client: &Client) -> Option<Credentials> {
    let data = client.get_data::<ClientState>().unwrap();
    if data.credentials_unknown {
        return None;
    }

    client.get_credentials(dh).ok()
}

pub fn ensure_min_max_size(mut x: i32, min_size: i32, max_size: i32) -> i32 {
    if max_size > 0 {
        x = min(x, max_size);
    }
    if min_size > 0 {
        x = max(x, min_size);
    }
    x
}

pub fn ensure_min_max_size_maybe_zero(x: i32, min_size: i32, max_size: i32) -> i32 {
    if x != 0 {
        ensure_min_max_size(x, min_size, max_size)
    } else if min_size > 0 && min_size == max_size {
        min_size
    } else {
        0
    }
}

pub fn clamp_preferring_top_left_in_area(
    area: Rectangle<f64, Logical>,
    rect: &mut Rectangle<f64, Logical>,
) {
    rect.loc.x = f64::min(rect.loc.x, area.loc.x + area.size.w - rect.size.w);
    rect.loc.y = f64::min(rect.loc.y, area.loc.y + area.size.h - rect.size.h);

    // Clamp by top and left last so it takes precedence.
    rect.loc.x = f64::max(rect.loc.x, area.loc.x);
    rect.loc.y = f64::max(rect.loc.y, area.loc.y);
}

pub fn center_preferring_top_left_in_area(
    area: Rectangle<f64, Logical>,
    size: Size<f64, Logical>,
) -> Point<f64, Logical> {
    let area_size = area.size.to_point();
    let size = size.to_point();
    let mut offset = (area_size - size).downscale(2.);
    offset.x = f64::max(offset.x, 0.);
    offset.y = f64::max(offset.y, 0.);
    area.loc + offset
}

pub fn baba_is_float_offset(now: Duration, view_height: f64) -> f64 {
    let now = now.as_secs_f64();
    let amplitude = view_height / 96.;
    amplitude * ((f64::consts::TAU * now / 3.6).sin() - 1.)
}

/// Run one of the shell's own notification actions.
///
/// gnome-shell attaches these as in-process closures; ours arrive as data across the
/// notification store's plain-data seam. Both end up in the same two GIO calls
/// (`js/ui/screenshot.js:2400-2418`).
pub fn run_shell_notification_action(action: &crate::notifications::ShellAction) {
    use gio::prelude::AppInfoExt as _;

    use crate::notifications::ShellAction;

    match action {
        ShellAction::OpenFile(path) => {
            let uri = match gio::glib::filename_to_uri(path, None) {
                Ok(uri) => uri,
                Err(err) => {
                    warn!("error making a uri for {path:?}: {err:?}");
                    return;
                }
            };
            if let Err(err) =
                gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
            {
                warn!("error opening {uri}: {err:?}");
            }
        }
        ShellAction::ShowInFiles(path) => {
            // GNOME hands the *file* to the directory handler, which is what makes Nautilus open
            // the containing folder with it selected rather than trying to display the PNG.
            let Some(app) = gio::AppInfo::default_for_type("inode/directory", false) else {
                // Null e.g. in a toolbox without nautilus — gnome-shell logs and gives up too.
                warn!("no default app for inode/directory; not showing the file");
                return;
            };
            let file = gio::File::for_path(path);
            if let Err(err) = app.launch(&[file], gio::AppLaunchContext::NONE) {
                warn!("error showing {path:?} in the file manager: {err:?}");
            }
        }
    }
}

/// A user's login name and real name, as the passwd database has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub name: String,
    /// The first GECOS field, which is what AccountsService itself seeds `real-name` from. Empty
    /// when the account has none.
    pub real_name: String,
}

/// Look up a login name in the passwd database, or `None` if there is no such user.
///
/// `getpwnam_r` for the same reason [`passwd_entry`] uses `getpwuid_r`.
pub fn passwd_entry_by_name(name: &str) -> Option<PasswdEntry> {
    let name = CString::new(name).ok()?;
    let mut buf = [0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = null_mut();

    // SAFETY: as `passwd_entry`, plus `name` is a NUL-terminated string that outlives the call.
    let rc = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    Some(passwd_fields(&pwd))
}

/// Look up a uid in the passwd database, or `None` if there is no such user.
///
/// `getpwuid_r` rather than `getpwuid`: the plain version returns a pointer into one static buffer
/// shared by the whole process, so two threads asking at once hand each other's answers back. That
/// is not hypothetical here — the polkit agent resolves identities on a D-Bus executor thread while
/// the compositor thread is free to ask about its own user. glibc's own callers use the `_r` form
/// for the same reason (`shell-polkit-authentication-agent.c:225`).
pub fn passwd_entry(uid: u32) -> Option<PasswdEntry> {
    // 4096 is what gnome-shell allocates for the same call. `getpwuid_r` reports ERANGE if a
    // record needs more, and we treat that as "no answer" rather than growing: a passwd entry
    // that large is malformed, not merely long.
    let mut buf = [0 as libc::c_char; 4096];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = null_mut();

    // SAFETY: `pwd`, `buf` and `result` are live for the call and sized as `getpwuid_r` requires.
    // The strings it writes point into `buf`, and we copy them out before returning.
    let rc = unsafe {
        libc::getpwuid_r(
            uid as libc::uid_t,
            &mut pwd,
            buf.as_mut_ptr(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    Some(passwd_fields(&pwd))
}

/// Copy the two strings we want out of a filled-in `passwd`.
fn passwd_fields(pwd: &libc::passwd) -> PasswdEntry {
    // SAFETY: the lookup succeeded, so these point into the caller's still-live buffer.
    let cstr = |p: *const libc::c_char| unsafe {
        if p.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };

    PasswdEntry {
        name: cstr(pwd.pw_name),
        // GECOS is comma-separated; the first field is the full name.
        real_name: cstr(pwd.pw_gecos)
            .split(',')
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

#[inline(never)]
pub fn cause_panic() {
    let a = Duration::from_secs(1);
    let b = Duration::from_secs(2);
    let _ = a - b;
}

/// A bare `Output` of the given logical size, for unit tests that need somewhere to put
/// geometry but no compositor around it. Not on the global space and not in any layout — for
/// anything that has to be laid out or rendered, use `Fixture`.
#[cfg(test)]
pub fn test_output(w: i32, h: i32) -> Output {
    let output = Output::new(
        "test".to_owned(),
        output::PhysicalProperties {
            size: Size::from((w, h)),
            subpixel: output::Subpixel::Unknown,
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
        },
    );
    output.change_current_state(
        Some(output::Mode {
            size: Size::from((w, h)),
            refresh: 60_000,
        }),
        None,
        None,
        None,
    );
    output.set_preferred(output::Mode {
        size: Size::from((w, h)),
        refresh: 60_000,
    });
    output
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;

    use super::*;

    /// A crop takes the requested rectangle, not the top-left corner of it.
    ///
    /// A stride bug here is invisible in a square test image and in any image whose crop starts at
    /// the origin, so the fixture is deliberately non-square and the crop deliberately offset.
    #[test]
    fn a_crop_takes_the_rectangle_it_was_given() {
        // 4x3, one distinct byte per pixel in the red channel: pixel (x, y) is y * 4 + x.
        let size = Size::<i32, Physical>::from((4, 3));
        let mut pixels = Vec::new();
        for y in 0..3 {
            for x in 0..4 {
                pixels.extend_from_slice(&[(y * 4 + x) as u8, 0, 0, 255]);
            }
        }

        let area = Rectangle::new(Point::from((1, 1)), Size::from((2, 2)));
        let (cropped_size, cropped) = crop_rgba8(size, &pixels, area).expect("a crop");

        assert_eq!(cropped_size, Size::from((2, 2)));
        let reds: Vec<u8> = cropped.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        assert_eq!(reds, vec![5, 6, 9, 10], "rows 1..3, columns 1..3");
    }

    /// An area running off the edge returns the part that exists; one entirely off it is an error,
    /// because there is no image to hand back.
    #[test]
    fn a_crop_clamps_to_the_screen() {
        let size = Size::<i32, Physical>::from((4, 3));
        let pixels = vec![0u8; 4 * 3 * 4];

        let over = Rectangle::new(Point::from((2, 2)), Size::from((10, 10)));
        let (clamped, _) = crop_rgba8(size, &pixels, over).expect("the part on screen");
        assert_eq!(clamped, Size::from((2, 1)));

        let off = Rectangle::new(Point::from((100, 100)), Size::from((4, 4)));
        assert!(crop_rgba8(size, &pixels, off).is_err());
    }

    #[test]
    fn test_format_diagonal() {
        assert_snapshot!(format_diagonal(12.11), @"12.1″");
        assert_snapshot!(format_diagonal(13.28), @"13.3″");
        assert_snapshot!(format_diagonal(15.6), @"15.6″");
        assert_snapshot!(format_diagonal(23.2), @"23″");
        assert_snapshot!(format_diagonal(24.8), @"25″");
    }

    #[test]
    fn test_clamp_preferring_top_left() {
        fn check(
            (ax, ay, aw, ah): (i32, i32, i32, i32),
            (rx, ry, rw, rh): (i32, i32, i32, i32),
            (ex, ey): (i32, i32),
        ) {
            let area = Rectangle::new(Point::from((ax, ay)), Size::from((aw, ah))).to_f64();
            let mut rect = Rectangle::new(Point::from((rx, ry)), Size::from((rw, rh))).to_f64();
            clamp_preferring_top_left_in_area(area, &mut rect);
            assert_eq!(rect.loc, Point::from((ex, ey)).to_f64());
        }

        check((0, 0, 10, 20), (2, 3, 4, 5), (2, 3));
        check((0, 0, 10, 20), (-2, 3, 4, 5), (0, 3));
        check((0, 0, 10, 20), (2, -3, 4, 5), (2, 0));
        check((0, 0, 10, 20), (-2, -3, 4, 5), (0, 0));

        check((1, 1, 10, 20), (2, 3, 4, 5), (2, 3));
        check((1, 1, 10, 20), (-2, 3, 4, 5), (1, 3));
        check((1, 1, 10, 20), (2, -3, 4, 5), (2, 1));
        check((1, 1, 10, 20), (-2, -3, 4, 5), (1, 1));

        check((0, 0, 10, 20), (20, 3, 4, 5), (6, 3));
        check((0, 0, 10, 20), (2, 30, 4, 5), (2, 15));
        check((0, 0, 10, 20), (20, 30, 4, 5), (6, 15));

        check((0, 0, 10, 20), (20, 30, 40, 5), (0, 15));
        check((0, 0, 10, 20), (20, 30, 4, 50), (6, 0));
        check((0, 0, 10, 20), (20, 30, 40, 50), (0, 0));
    }
}
