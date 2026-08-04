// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::ffi::OsStr;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::{io, thread};

use atomic::Atomic;
use libc::{getrlimit, rlim_t, rlimit, setrlimit, RLIMIT_NOFILE};
use smithay::wayland::xdg_activation::XdgActivationToken;
use synoik_config::Environment;

use crate::utils::expand_home;

pub static REMOVE_ENV_RUST_BACKTRACE: AtomicBool = AtomicBool::new(false);
pub static REMOVE_ENV_RUST_LIB_BACKTRACE: AtomicBool = AtomicBool::new(false);
pub static CHILD_ENV: RwLock<Environment> = RwLock::new(Environment(Vec::new()));
pub static CHILD_DISPLAY: RwLock<Option<String>> = RwLock::new(None);

static ORIGINAL_NOFILE_RLIMIT_CUR: Atomic<rlim_t> = Atomic::new(0);
static ORIGINAL_NOFILE_RLIMIT_MAX: Atomic<rlim_t> = Atomic::new(0);

/// Increases the nofile rlimit to the maximum and stores the original value.
pub fn store_and_increase_nofile_rlimit() {
    let mut rlim = rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { getrlimit(RLIMIT_NOFILE, &mut rlim) } != 0 {
        let err = io::Error::last_os_error();
        warn!("error getting nofile rlimit: {err:?}");
        return;
    }

    ORIGINAL_NOFILE_RLIMIT_CUR.store(rlim.rlim_cur, Ordering::SeqCst);
    ORIGINAL_NOFILE_RLIMIT_MAX.store(rlim.rlim_max, Ordering::SeqCst);

    trace!(
        "changing nofile rlimit from {} to {}",
        rlim.rlim_cur,
        rlim.rlim_max
    );
    rlim.rlim_cur = rlim.rlim_max;

    if unsafe { setrlimit(RLIMIT_NOFILE, &rlim) } != 0 {
        let err = io::Error::last_os_error();
        warn!("error setting nofile rlimit: {err:?}");
    }
}

/// Restores the original nofile rlimit.
pub fn restore_nofile_rlimit() {
    let rlim_cur = ORIGINAL_NOFILE_RLIMIT_CUR.load(Ordering::SeqCst);
    let rlim_max = ORIGINAL_NOFILE_RLIMIT_MAX.load(Ordering::SeqCst);

    if rlim_cur == 0 {
        return;
    }

    let rlim = rlimit { rlim_cur, rlim_max };
    unsafe { setrlimit(RLIMIT_NOFILE, &rlim) };
}

/// Spawns the command to run independently of the compositor.
pub fn spawn<T: AsRef<OsStr> + Send + 'static>(command: Vec<T>, token: Option<XdgActivationToken>) {
    let _span = tracy_client::span!();

    if command.is_empty() {
        return;
    }

    // Spawning and waiting takes some milliseconds, so do it in a thread.
    let res = thread::Builder::new()
        .name("Command Spawner".to_owned())
        .spawn(move || {
            let (command, args) = command.split_first().unwrap();
            spawn_sync(command, args, token);
        });

    if let Err(err) = res {
        warn!("error spawning a thread to spawn the command: {err:?}");
    }
}

/// Spawns the command through the shell.
///
/// We hardcode `sh -c`, consistent with other compositors:
///
/// - https://github.com/swaywm/sway/blob/b3dcde8d69c3f1304b076968a7a64f54d0c958be/sway/commands/exec_always.c#L64
/// - https://github.com/hyprwm/Hyprland/blob/1ac1ff457ab8ef1ae6a8f2ab17ee7965adfa729f/src/managers/KeybindManager.cpp#L987
pub fn spawn_sh(command: String, token: Option<XdgActivationToken>) {
    spawn(vec![String::from("sh"), String::from("-c"), command], token);
}

fn spawn_sync(
    command: impl AsRef<OsStr>,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
    token: Option<XdgActivationToken>,
) {
    let _span = tracy_client::span!();

    let mut command = command.as_ref();

    // Expand `~` at the start.
    let expanded = expand_home(Path::new(command));
    match &expanded {
        Ok(Some(expanded)) => command = expanded.as_ref(),
        Ok(None) => (),
        Err(err) => {
            warn!("error expanding ~: {err:?}");
        }
    }

    let mut process = Command::new(command);
    process
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // Remove RUST_BACKTRACE and RUST_LIB_BACKTRACE from the environment if needed.
    if REMOVE_ENV_RUST_BACKTRACE.load(Ordering::Relaxed) {
        process.env_remove("RUST_BACKTRACE");
    }
    if REMOVE_ENV_RUST_LIB_BACKTRACE.load(Ordering::Relaxed) {
        process.env_remove("RUST_LIB_BACKTRACE");
    }

    // Remove the systemd NOTIFY_SOCKET variable.
    process.env_remove("NOTIFY_SOCKET");

    // Set DISPLAY if needed.
    let display = CHILD_DISPLAY.read().unwrap();
    if let Some(display) = &*display {
        process.env("DISPLAY", display);
    } else {
        process.env_remove("DISPLAY");
    }

    // Set configured environment.
    let env = CHILD_ENV.read().unwrap();
    for var in &env.0 {
        if let Some(value) = &var.value {
            process.env(&var.name, value);
        } else {
            process.env_remove(&var.name);
        }
    }
    drop(env);

    if let Some(token) = token.as_ref() {
        process.env("XDG_ACTIVATION_TOKEN", token.as_str());
        process.env("DESKTOP_STARTUP_ID", token.as_str());
    }

    unsafe { process.pre_exec(crate::utils::signals::unblock_all) };

    let Some(mut child) = do_spawn(command, process) else {
        return;
    };

    match child.wait() {
        Ok(status) => {
            if !status.success() {
                warn!("child did not exit successfully: {status:?}");
            }
        }
        Err(err) => {
            warn!("error waiting for child: {err:?}");
        }
    }
}

#[cfg(not(feature = "systemd"))]
fn do_spawn(command: &OsStr, mut process: Command) -> Option<Child> {
    unsafe {
        // Double-fork to avoid having to waitpid the child.
        process.pre_exec(move || {
            match libc::fork() {
                -1 => return Err(io::Error::last_os_error()),
                0 => (),
                _ => libc::_exit(0),
            }

            restore_nofile_rlimit();

            Ok(())
        });
    }

    let child = match process.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!("error spawning {command:?}: {err:?}");
            return None;
        }
    };

    Some(child)
}

#[cfg(feature = "systemd")]
use systemd::do_spawn;

/// Move an already-running application into its own transient systemd scope.
///
/// For apps we *spawn* this happens inside [`spawn`]; this is the other path, an app launched
/// through GIO from the dash/grid/search, where the process already exists and only needs
/// re-parenting. Without it the app stays in the compositor's own cgroup — which for us is
/// `org.gnome.Shell@user.service` — where it is a stray in a stopping unit at logout, reaped by
/// that unit's `TimeoutStopSec` with SIGABRT rather than asked to quit.
///
/// GNOME does exactly this, from the launch context's `launched` signal:
/// `shell-global.c:1182-1207` calls libgnome-desktop's `gnome_start_systemd_scope`, whose unit
/// name is `app-gnome-<id>-<pid>.scope` — matched here, including the prefix, since we are the
/// shell those tools expect to be looking at.
///
/// A no-op without the `systemd` feature, and without being a systemd service ourselves.
#[cfg(feature = "systemd")]
pub fn start_app_scope(app_id: &str, pid: u32) {
    systemd::start_app_scope(app_id, pid);
}

#[cfg(not(feature = "systemd"))]
pub fn start_app_scope(_app_id: &str, _pid: u32) {}

#[cfg(feature = "systemd")]
mod systemd {
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

    use smithay::reexports::rustix;
    use smithay::reexports::rustix::io::{close, read, retry_on_intr, write};
    use smithay::reexports::rustix::pipe::{pipe_with, PipeFlags};

    use super::*;

    pub fn do_spawn(command: &OsStr, mut process: Command) -> Option<Child> {
        #[cfg(target_env = "gnu")]
        use libc::close_range;
        #[cfg(target_os = "openbsd")]
        use libc::closefrom;

        #[cfg(not(target_env = "gnu"))] // musl
        pub fn close_range(first: libc::c_uint, last: libc::c_uint, flags: libc::c_uint) -> i64 {
            unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    first as usize,
                    last as usize,
                    flags as usize,
                )
            }
        }

        // When running as a systemd session, we want to put children into their own transient
        // scopes in order to separate them from the synoik process. This is helpful for
        // example to prevent the OOM killer from taking down synoik together with a
        // misbehaving client.
        //
        // Putting a child into a scope is done by calling systemd's StartTransientUnit D-Bus method
        // with a PID. Unfortunately, there seems to be a race in systemd where if the child exits
        // at just the right time, the transient unit will be created but empty, so it will
        // linger around forever.
        //
        // To prevent this, we'll use our double-fork (done for a separate reason) to help. In our
        // intermediate child we will send back the grandchild PID, and in synoik we will create a
        // transient scope with both our intermediate child and the grandchild PIDs set. Only then
        // we will signal our intermediate child to exit. This way, even if the grandchild
        // exits quickly, a non-empty scope will be created (with just our intermediate
        // child), then cleaned up when our intermediate child exits.

        // Make a pipe to receive the grandchild PID.

        let (pipe_pid_read, pipe_pid_write) = pipe_with(PipeFlags::CLOEXEC)
            .map_err(|err| {
                warn!("error creating a pipe to transfer child PID: {err:?}");
            })
            .ok()
            .unzip();
        // Make a pipe to wait in the intermediate child.
        let (pipe_wait_read, pipe_wait_write) = pipe_with(PipeFlags::CLOEXEC)
            .map_err(|err| {
                warn!("error creating a pipe for child to wait on: {err:?}");
            })
            .ok()
            .unzip();

        unsafe {
            // The fds will be duplicated after a fork and closed on exec or exit automatically. Get
            // the raw fd inside so that it's not closed any extra times.
            let mut pipe_pid_read_fd = pipe_pid_read.as_ref().map(|fd| fd.as_raw_fd());
            let mut pipe_pid_write_fd = pipe_pid_write.as_ref().map(|fd| fd.as_raw_fd());
            let mut pipe_wait_read_fd = pipe_wait_read.as_ref().map(|fd| fd.as_raw_fd());
            let mut pipe_wait_write_fd = pipe_wait_write.as_ref().map(|fd| fd.as_raw_fd());

            // Double-fork to avoid having to waitpid the child.
            process.pre_exec(move || {
                // Close FDs that we don't need. Especially important for the write ones to unblock
                // the readers.
                if let Some(fd) = pipe_pid_read_fd.take() {
                    close(fd);
                }
                if let Some(fd) = pipe_wait_write_fd.take() {
                    close(fd);
                }

                // Convert the FDs to OwnedFd, which will close them in all of our fork paths.
                let pipe_pid_write = pipe_pid_write_fd.take().map(|fd| OwnedFd::from_raw_fd(fd));
                let pipe_wait_read = pipe_wait_read_fd.take().map(|fd| OwnedFd::from_raw_fd(fd));

                match libc::fork() {
                    -1 => return Err(io::Error::last_os_error()),
                    0 => (),
                    grandchild_pid => {
                        // Send back the PID.
                        if let Some(pipe) = pipe_pid_write {
                            let _ = write_all(pipe, &grandchild_pid.to_ne_bytes());
                        }

                        // Wait until the parent signals us to exit.
                        if let Some(pipe) = pipe_wait_read {
                            // We're going to exit afterwards. Close all other FDs to allow
                            // Command::spawn() to return in the parent process.
                            #[cfg(not(target_os = "openbsd"))]
                            {
                                let raw = pipe.as_raw_fd() as u32;
                                let _ = close_range(0, raw - 1, 0);
                                let _ = close_range(raw + 1, !0, 0);
                            }
                            #[cfg(target_os = "openbsd")]
                            {
                                let raw = pipe.as_raw_fd();
                                for fd in 0..raw {
                                    close(fd);
                                }
                                closefrom(raw + 1);
                            }

                            let _ = read_all(pipe, &mut [0]);
                        }

                        libc::_exit(0)
                    }
                }

                restore_nofile_rlimit();

                Ok(())
            });
        }

        let child = match process.spawn() {
            Ok(child) => child,
            Err(err) => {
                warn!("error spawning {command:?}: {err:?}");
                return None;
            }
        };

        drop(pipe_pid_write);
        drop(pipe_wait_read);

        // Wait for the grandchild PID.
        if let Some(pipe) = pipe_pid_read {
            let mut buf = [0; 4];
            match read_all(pipe, &mut buf) {
                Ok(()) => {
                    let pid = i32::from_ne_bytes(buf);
                    trace!("spawned PID: {pid}");

                    // Start a systemd scope for the grandchild.
                    if let Err(err) = start_systemd_scope(command, child.id(), pid as u32) {
                        trace!("error starting systemd scope for spawned command: {err:?}");
                    }
                }
                Err(err) => {
                    warn!("error reading child PID: {err:?}");
                }
            }
        }

        // Signal the intermediate child to exit now that we're done trying to creating a systemd
        // scope.
        trace!("signaling child to exit");
        drop(pipe_wait_write);

        Some(child)
    }

    fn write_all(fd: impl AsFd, buf: &[u8]) -> rustix::io::Result<()> {
        let mut written = 0;
        loop {
            let n = retry_on_intr(|| write(&fd, &buf[written..]))?;
            if n == 0 {
                return Err(rustix::io::Errno::CANCELED);
            }

            written += n;
            if written == buf.len() {
                return Ok(());
            }
        }
    }

    fn read_all(fd: impl AsFd, buf: &mut [u8]) -> rustix::io::Result<()> {
        let mut start = 0;
        loop {
            let n = retry_on_intr(|| read(&fd, &mut buf[start..]))?;
            if n == 0 {
                return Err(rustix::io::Errno::CANCELED);
            }

            start += n;
            if start == buf.len() {
                return Ok(());
            }
        }
    }

    /// Puts a (newly spawned) pid into a transient systemd scope.
    ///
    /// This separates the pid from the compositor scope, which for example prevents the OOM killer
    /// from bringing down the compositor together with a misbehaving client.
    fn start_systemd_scope(
        name: &OsStr,
        intermediate_pid: u32,
        child_pid: u32,
    ) -> anyhow::Result<()> {
        use std::os::unix::ffi::OsStrExt;

        use crate::utils::IS_SYSTEMD_SERVICE;

        // We only start transient scopes if we're a systemd service ourselves.
        if !IS_SYSTEMD_SERVICE.load(Ordering::Relaxed) {
            return Ok(());
        }

        let _span = tracy_client::span!();

        // Extract the basename.
        let name = Path::new(name).file_name().unwrap_or(name);
        let scope_name = format!(
            "app-synoik-{}-{child_pid}.scope",
            escape_unit_name(name.as_bytes())
        );

        // Wait for the job: the intermediate child must not exit before the scope exists, or the
        // scope is created around a PID that is already gone.
        start_transient_scope(
            &scope_name,
            SCOPE_DESCRIPTION,
            &[intermediate_pid, child_pid],
            true,
        )
    }

    /// Escape a name for use inside a systemd unit name, similarly to libgnome-desktop, which says
    /// it had adapted this from systemd source.
    fn escape_unit_name(name: &[u8]) -> String {
        use std::fmt::Write as _;

        let mut escaped = String::with_capacity(name.len());
        for &c in name {
            if c.is_ascii_alphanumeric() || matches!(c, b':' | b'_' | b'.') {
                escaped.push(char::from(c));
            } else {
                let _ = write!(escaped, "\\x{c:02x}");
            }
        }
        escaped
    }

    /// What `systemctl` shows for a scope we started, matching the shape of GNOME's
    /// `"Application launched by %s"` (libgnome-desktop's `gnome_start_systemd_scope`).
    const SCOPE_DESCRIPTION: &str = "Application launched by synoik";

    /// Ask systemd to put `pids` into a new transient scope called `scope_name`.
    ///
    /// `wait_for_job` blocks until systemd reports the unit started; callers that have nothing
    /// waiting on the scope's existence should pass `false` and not pay the round trip.
    fn start_transient_scope(
        scope_name: &str,
        description: &str,
        pids: &[u32],
        wait_for_job: bool,
    ) -> anyhow::Result<()> {
        use std::sync::OnceLock;

        use anyhow::Context;
        use zbus::zvariant::{OwnedObjectPath, Value};

        // Ask systemd to start a transient scope.
        static CONNECTION: OnceLock<zbus::Result<zbus::blocking::Connection>> = OnceLock::new();
        let conn = CONNECTION
            .get_or_init(zbus::blocking::Connection::session)
            .clone()
            .context("error connecting to session bus")?;

        let proxy = zbus::blocking::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .context("error creating a Proxy")?;

        // Subscribe before the call, so a job that finishes immediately is not missed.
        let signals = wait_for_job
            .then(|| proxy.receive_signal("JobRemoved"))
            .transpose()
            .context("error creating a signal iterator")?;

        // `PartOf=graphical-session.target` is the property that gets the app a SIGTERM when the
        // session ends, instead of being a stray in a stopping unit. GNOME gets it from a drop-in
        // gnome-session ships for the `app-gnome-` unit-name prefix
        // (`/usr/lib/systemd/user/app-gnome-.scope.d/override.conf`), which our `app-gnome-*`
        // scopes do inherit — verified live, `DropInPaths` names that file. We set it here anyway:
        // it also covers the `app-synoik-*` prefix, which matches no drop-in, and it does not leave
        // the property that makes logout work resting on a file from the package we intend to
        // replace. `TimeoutStopSec` matches that drop-in's 5s.
        let properties: &[_] = &[
            ("Description", Value::new(description)),
            ("PIDs", Value::new(pids)),
            ("CollectMode", Value::new("inactive-or-failed")),
            ("PartOf", Value::new(vec!["graphical-session.target"])),
            ("TimeoutStopUSec", Value::new(5_000_000u64)),
        ];
        let aux: &[(&str, &[(&str, Value)])] = &[];

        let job: OwnedObjectPath = proxy
            .call("StartTransientUnit", &(scope_name, "fail", properties, aux))
            .context("error calling StartTransientUnit")?;

        let Some(signals) = signals else {
            return Ok(());
        };

        trace!("waiting for JobRemoved");
        for message in signals {
            let body = message.body();
            let body: (u32, OwnedObjectPath, &str, &str) =
                body.deserialize().context("error parsing signal")?;

            if body.1 == job {
                // Our transient unit had started, we're good to exit the intermediate child.
                break;
            }
        }

        Ok(())
    }

    /// The unit-name patterns covering every scope an app of ours can end up in: `app-gnome-*`
    /// from [`start_app_scope`] (GNOME's prefix, which we match on purpose), `app-synoik-*` from
    /// [`start_systemd_scope`] (the `spawn` path), and `app-flatpak-*`, which is **not** ours to
    /// create but is where a flatpak app actually lives.
    ///
    /// That last one is why this is a pattern list and not a registry of what we launched. We do
    /// start an `app-gnome-<id>-<pid>.scope` for a flatpak app — and then `flatpak run` hands the
    /// real processes to a scope of its own and exits, so ours goes empty and
    /// `CollectMode=inactive-or-failed` takes it away. By logout the only unit holding the app is
    /// flatpak's, under a prefix we did not match: measured 2026-08-03, OBS was never asked to quit
    /// until the `graphical-session.target` teardown reached it, five seconds into a drain that had
    /// already given up on it. It is the same class of unit either way —
    /// `/usr/lib/systemd/user/app-flatpak-.scope.d/` carries the same
    /// `PartOf=graphical-session.target` and `TimeoutStopSec=5s` drop-in as `app-gnome-`— so
    /// all this changes is *when* it is asked.
    pub fn start_app_scope(app_id: &str, pid: u32) {
        use crate::utils::IS_SYSTEMD_SERVICE;

        if !IS_SYSTEMD_SERVICE.load(Ordering::Relaxed) {
            return;
        }

        let scope_name = format!(
            "app-gnome-{}-{pid}.scope",
            escape_unit_name(app_id.as_bytes())
        );

        // Off-thread and unwaited, like GNOME's ("Start async request; we don't care about the
        // result"). This runs from the launch path, which is the compositor thread; a
        // StartTransientUnit round trip there would be a frame's worth of blocking D-Bus for a
        // result nothing reads.
        let spawned = thread::Builder::new()
            .name("app scope".to_owned())
            .spawn(move || {
                if let Err(err) =
                    start_transient_scope(&scope_name, SCOPE_DESCRIPTION, &[pid], false)
                {
                    // Losing the race against a process that exits immediately is normal.
                    trace!("error starting the app scope: {err:?}");
                }
            });
        if let Err(err) = spawned {
            warn!("error spawning the app scope thread: {err:?}");
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A malformed unit name is rejected by systemd, not by us — the scope silently fails to
        /// appear and the app stays in our cgroup, which is exactly the bug this all exists to fix.
        /// So pin the escaping against the characters real app ids and argv[0]s actually contain.
        #[test]
        fn app_ids_survive_escaping_into_a_unit_name() {
            // Dots are legal in unit names and must NOT be escaped, or every reverse-DNS app id
            // (which is most of them) turns into noise.
            assert_eq!(
                escape_unit_name(b"org.mozilla.firefox.desktop"),
                "org.mozilla.firefox.desktop"
            );
            assert_eq!(escape_unit_name(b"kitty"), "kitty");
            assert_eq!(escape_unit_name(b"gnome-terminal"), "gnome\\x2dterminal");
            assert_eq!(escape_unit_name(b"my app"), "my\\x20app");
            assert_eq!(escape_unit_name(b"a/b"), "a\\x2fb");
            // Non-ASCII goes byte by byte rather than by char, since unit names are bytes.
            assert_eq!(escape_unit_name("é".as_bytes()), "\\xc3\\xa9");
        }
    }
}
