#[macro_use]
extern crate tracing;

use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::{env, mem, thread};

use calloop::EventLoop;
use clap::{CommandFactory, Parser};
use clap_complete::Shell;
use clap_complete_nushell::Nushell;
use directories::ProjectDirs;
use niri::backend::BackendMode;
use niri::cli::{Cli, CompletionShell, Sub};
#[cfg(feature = "dbus")]
use niri::dbus;
use niri::frame_log;
use niri::ipc::client::handle_msg;
use niri::niri::State;
use niri::utils::spawning::{
    spawn, spawn_sh, store_and_increase_nofile_rlimit, CHILD_DISPLAY, CHILD_ENV,
    REMOVE_ENV_RUST_BACKTRACE, REMOVE_ENV_RUST_LIB_BACKTRACE,
};
use niri::utils::{cause_panic, version, watcher, xwayland, IS_SYSTEMD_SERVICE};
use niri_config::{Config, ConfigPath};
use niri_ipc::socket::SOCKET_PATH_ENV;
use sd_notify::NotifyState;
use smithay::reexports::wayland_server::Display;
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str = "niri=debug,smithay::backend::renderer::gles=error";

#[cfg(feature = "profile-with-tracy-allocations")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<std::alloc::System> =
    tracy_client::ProfiledAllocator::new(std::alloc::System, 100);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    niri::gnome::init_collation();

    // Set backtrace defaults if not set.
    if env::var_os("RUST_BACKTRACE").is_none() {
        env::set_var("RUST_BACKTRACE", "1");
        REMOVE_ENV_RUST_BACKTRACE.store(true, Ordering::Relaxed);
    }
    if env::var_os("RUST_LIB_BACKTRACE").is_none() {
        env::set_var("RUST_LIB_BACKTRACE", "0");
        REMOVE_ENV_RUST_LIB_BACKTRACE.store(true, Ordering::Relaxed);
    }

    // Before ANY thread is spawned, so every one of them inherits the mask — a
    // thread that does not block these takes the signal instead of the signalfd,
    // and the default action for all of them is to kill the process. That is not
    // hypothetical: the `log-writer` thread below used to be created first, and it
    // swallowed SIGUSR1 (the frame-log dump) straight into a silent exit. The same
    // race applied to SIGINT/SIGTERM, where it would have skipped the clean
    // shutdown path.
    niri::utils::signals::block_early().unwrap();

    // Log through a writer thread, so emitting a line is a channel send instead of a `write(2)`.
    //
    // Under systemd our stderr is a journald socket, and writing to it blocks whenever journald
    // is behind — on a thread that has 16.67 ms to hand a frame to KMS. Measured at a realistic
    // 60 Hz it is only ~26 µs (p99 112 µs, worst 714 µs over 30 s), so this is not about the
    // steady-state cost; it is that the cost is *unbounded* under backpressure, and that the
    // `io::stderr()` lock serializes the compositor thread against every service thread we
    // deliberately moved off it. `NIRI_FRAME_LOG` also makes this per-frame, which means the
    // instrument was perturbing the very runs we take decisions from.
    //
    // Lossy: on overflow a line is dropped rather than the compositor stalled. Drops are counted
    // and reported by the frame-log summary (`frame_log::watch_dropped_lines`) — a hole in this
    // log has to announce itself, or an absent frame line reads as a frame that never happened.
    // `NIRI_LOG_BLOCKING=1` swaps in backpressure for when a run must not lose anything.
    let blocking = env::var_os("NIRI_LOG_BLOCKING").is_some();
    let (writer, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        // ~16k lines of slack. At the ~120 lines/s a fully instrumented session produces that is
        // over two minutes of buffer, which is far more journald hiccup than we have ever seen;
        // the cap exists so a runaway logger cannot grow without bound.
        .buffered_lines_limit(16_384)
        .lossy(!blocking)
        .thread_name("log-writer")
        .finish(io::stderr());
    // Held to the end of `main`: dropping the guard shuts the writer thread down and flushes what
    // is queued. Bound to a name rather than `_`, which would drop it here and take the log with
    // it. Note this cannot flush on `abort` — a crash loses whatever is still queued, which is the
    // price of not blocking.
    let _log_guard = guard;
    frame_log::watch_dropped_lines(writer.error_counter());

    let directives = env::var("RUST_LOG").unwrap_or_else(|_| DEFAULT_LOG_FILTER.to_owned());
    let env_filter = EnvFilter::builder().parse_lossy(directives);
    tracing_subscriber::fmt()
        .compact()
        .with_writer(writer)
        .with_env_filter(env_filter)
        .with_ansi_sanitization(false)
        .init();

    if env::var_os("NOTIFY_SOCKET").is_some() {
        IS_SYSTEMD_SERVICE.store(true, Ordering::Relaxed);

        #[cfg(not(feature = "systemd"))]
        warn!(
            "running as a systemd service, but systemd support is compiled out. \
             Are you sure you did not forget to set `--features systemd`?"
        );
    }

    let cli = Cli::parse();

    if cli.session {
        // If we're starting as a session, assume that the intention is to start on a TTY unless
        // this is a WSL environment. Remove DISPLAY, WAYLAND_DISPLAY or WAYLAND_SOCKET from our
        // environment if they are set: they'd be inherited by everything we spawn, pointing our
        // own clients at whatever display we were launched from. (They also used to select the
        // winit backend, which no longer exists.)
        if env::var_os("WSL_DISTRO_NAME").is_none() {
            if env::var_os("DISPLAY").is_some() {
                warn!("running as a session but DISPLAY is set, removing it");
                env::remove_var("DISPLAY");
            }
            if env::var_os("WAYLAND_DISPLAY").is_some() {
                warn!("running as a session but WAYLAND_DISPLAY is set, removing it");
                env::remove_var("WAYLAND_DISPLAY");
            }
            if env::var_os("WAYLAND_SOCKET").is_some() {
                warn!("running as a session but WAYLAND_SOCKET is set, removing it");
                env::remove_var("WAYLAND_SOCKET");
            }
        }

        // The current desktop drives xdg-desktop-portal backend selection and
        // gio's per-session GSettings defaults (e.g. `[org.gnome.mutter:GNOME]`
        // enables edge-tiling). Keep whatever the session manager set — GDM
        // exports the session file's DesktopNames, "GNOME" for the GNOME
        // session we replace gnome-shell in — and present as GNOME otherwise.
        if env::var_os("XDG_CURRENT_DESKTOP").is_none() {
            env::set_var("XDG_CURRENT_DESKTOP", "GNOME");
        }
        // Ensure the session type is set to Wayland for xdg-autostart and Qt apps.
        env::set_var("XDG_SESSION_TYPE", "wayland");
    }

    // Handle subcommands.
    if let Some(subcommand) = cli.subcommand {
        // These are short-lived clients, not the compositor: they spawn no threads,
        // install no signalfd, and have no clean-shutdown path to protect. Leaving
        // them under the compositor's mask makes Ctrl-C do nothing at all — which
        // matters most in exactly the case you would use it, a `niri msg` blocked
        // on the socket of a wedged compositor.
        niri::utils::signals::unblock_all().unwrap();

        match subcommand {
            Sub::Validate { config } => {
                tracy_client::Client::start();

                config_path(config).load().config?;
                info!("config is valid");
                return Ok(());
            }
            Sub::Msg { msg, json } => {
                handle_msg(msg, json)?;
                return Ok(());
            }
            Sub::Panic => cause_panic(),
            Sub::Completions { shell } => {
                match shell {
                    CompletionShell::Nushell => {
                        clap_complete::generate(
                            Nushell,
                            &mut Cli::command(),
                            "niri",
                            &mut io::stdout(),
                        );
                    }
                    other => {
                        let generator = Shell::try_from(other).unwrap();
                        clap_complete::generate(
                            generator,
                            &mut Cli::command(),
                            "niri",
                            &mut io::stdout(),
                        );
                    }
                }
                return Ok(());
            }
        }
    }

    // Avoid starting Tracy for the `niri msg` code path since starting/stopping Tracy is a bit
    // slow.
    tracy_client::Client::start();

    info!("starting version {}", &version());

    // Load the config.
    let config_path = config_path(cli.config);
    env::remove_var("NIRI_CONFIG");
    let (config_created_at, config_load_result) = config_path.load_or_create();
    let config_errored = config_load_result.config.is_err();
    let mut config = config_load_result.config.unwrap_or_else(|err| {
        warn!("{err:?}");
        Config::load_default()
    });
    let config_includes = config_load_result.includes;

    let spawn_at_startup = mem::take(&mut config.spawn_at_startup);
    let spawn_sh_at_startup = mem::take(&mut config.spawn_sh_at_startup);
    *CHILD_ENV.write().unwrap() = mem::take(&mut config.environment);

    store_and_increase_nofile_rlimit();

    // Read the fonts off the disk while the event loop, the display and the backend are being
    // built, so the first frame does not pay for it. Detached on purpose: nothing waits on the
    // result, and if it has not finished by the first shape that call simply does the work itself.
    // That last part is load-bearing rather than incidental — making the first shape wait for this
    // thread instead moved 310ms into time-to-first-frame on the live seat.
    thread::Builder::new()
        .name("font prewarm".to_owned())
        .spawn(niri_vk::text::prewarm)
        .map_err(|err| warn!("error spawning the font prewarm thread: {err:?}"))
        .ok();

    // Create the main event loop.
    let mut event_loop = EventLoop::<State>::try_new().unwrap();

    // Handle Ctrl+C and other signals.
    niri::utils::signals::listen(&event_loop.handle());

    // Create the compositor.
    let display = Display::new().unwrap();

    // Increase the buffer size so that it's harder to crash a frozen client with a 1000 Hz mouse.
    set_default_max_buffer_size(&display, 1024 * 1024);

    let mode = if cli.headless {
        BackendMode::Headless
    } else {
        BackendMode::Auto
    };
    let mut state = State::new(
        config,
        event_loop.handle(),
        event_loop.get_signal(),
        display,
        mode,
        true,
        cli.session,
    )
    .unwrap();

    if cli.headless {
        // A renderer lets clients actually draw (and screencasting work), but
        // isn't required for driving the compositor logic over IPC.
        if let Err(err) = state.backend.headless().add_renderer() {
            warn!("error creating headless renderer, running without one: {err:?}");
        }
        // `NIRI_HEADLESS_MODE=WxH` sizes the virtual output. The headless backend
        // advertises exactly one (custom) mode, so `niri msg output … mode` cannot reach
        // any other shape — and chrome that adapts to the canvas has to be judged on a
        // canvas, at the mode+scale of the display being reproduced.
        let size = env::var("NIRI_HEADLESS_MODE")
            .ok()
            .and_then(|s| {
                let (w, h) = s.split_once(['x', 'X'])?;
                Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
            })
            .unwrap_or((1920, 1080));
        let niri = &mut state.niri;
        state.backend.headless().add_output(niri, 1, size);
    }

    // Set WAYLAND_DISPLAY for children.
    let socket_name = state.niri.socket_name.as_deref().unwrap();
    env::set_var("WAYLAND_DISPLAY", socket_name);
    info!(
        "listening on Wayland socket: {}",
        socket_name.to_string_lossy()
    );

    // Set NIRI_SOCKET for children.
    if let Some(ipc) = &state.niri.ipc_server {
        let socket_path = ipc.socket_path.as_deref().unwrap();
        env::set_var(SOCKET_PATH_ENV, socket_path);
        info!("IPC listening on: {}", socket_path.to_string_lossy());
    }

    // Setup xwayland-satellite integration.
    xwayland::satellite::setup(&mut state);
    if let Some(satellite) = &state.niri.satellite {
        let name = satellite.display_name();
        *CHILD_DISPLAY.write().unwrap() = Some(name.to_owned());
        env::set_var("DISPLAY", name);
        info!("listening on X11 socket: {name}");
    } else {
        // Avoid spawning children in the host X11.
        env::remove_var("DISPLAY");
    }

    if cli.session {
        // We're starting as a session. Import our variables.
        import_environment();

        // Inhibit power key handling so we can suspend on it.
        #[cfg(feature = "dbus")]
        if !state.niri.config.borrow().input.disable_power_key_handling {
            if let Err(err) = state.niri.inhibit_power_key() {
                warn!("error inhibiting power key: {err:?}");
            }
        }
    }

    #[cfg(feature = "dbus")]
    dbus::DBusServers::start(&mut state, cli.session);

    // Default-sink volume/mute for the panel indicator + QS slider, from PipeWire.
    // The connection's loop is driven on the compositor's calloop; skip in headless
    // (IPC-only) runs.
    #[cfg(feature = "pipewire")]
    if !cli.headless {
        match niri::pipewire_audio::start(&event_loop.handle()) {
            Ok(pw) => {
                pw.pump();
                state.niri.pw_audio = Some(pw);
            }
            Err(err) => warn!("error starting PipeWire audio watcher: {err:?}"),
        }
    }

    #[cfg(feature = "dbus")]
    if cli.session {
        state.niri.a11y.start();
    }

    if env::var_os("NIRI_DISABLE_SYSTEM_MANAGER_NOTIFY").is_none_or(|x| x != "1") {
        // Notify systemd we're ready.
        if let Err(err) = sd_notify::notify(&[NotifyState::Ready]) {
            warn!("error notifying systemd: {err:?}");
        };

        // Send ready notification to the NOTIFY_FD file descriptor.
        if let Err(err) = notify_fd() {
            warn!("error notifying fd: {err:?}");
        }
    }

    watcher::setup(&mut state, &config_path, config_includes);

    // Spawn commands from cli and auto-start.
    spawn(cli.command, None);

    for elem in spawn_at_startup {
        spawn(elem.command, None);
    }
    for elem in spawn_sh_at_startup {
        spawn_sh(elem.command, None);
    }

    // Show the config error notification right away if needed.
    if config_errored {
        state.niri.config_error_notification.show();
        state.ipc_config_loaded(true);
    } else if let Some(path) = config_created_at {
        state.niri.config_error_notification.show_created(path);
    }

    // Run the compositor.
    event_loop
        .run(None, &mut state, |state| state.refresh_and_flush_clients())
        .unwrap();

    Ok(())
}

fn import_environment() {
    let variables = [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        SOCKET_PATH_ENV,
    ]
    .join(" ");

    let mut init_system_import = String::new();
    if cfg!(feature = "systemd") {
        write!(
            init_system_import,
            "systemctl --user import-environment {variables};"
        )
        .unwrap();
    }
    if cfg!(feature = "dinit") {
        write!(init_system_import, "dinitctl setenv {variables};").unwrap();
    }

    let rv = Command::new("/bin/sh")
        .args([
            "-c",
            &format!(
                "{init_system_import}\
                 hash dbus-update-activation-environment 2>/dev/null && \
                 dbus-update-activation-environment {variables}"
            ),
        ])
        .spawn();
    // Wait for the import process to complete, otherwise services will start too fast without
    // environment variables available.
    match rv {
        Ok(mut child) => match child.wait() {
            Ok(status) => {
                if !status.success() {
                    warn!("import environment shell exited with {status}");
                }
            }
            Err(err) => {
                warn!("error waiting for import environment shell: {err:?}");
            }
        },
        Err(err) => {
            warn!("error spawning shell to import environment: {err:?}");
        }
    }
}

fn env_config_path() -> Option<PathBuf> {
    env::var_os("NIRI_CONFIG")
        .filter(|x| !x.is_empty())
        .map(PathBuf::from)
}

fn default_config_path() -> Option<PathBuf> {
    let Some(dirs) = ProjectDirs::from("", "", "niri") else {
        warn!("error retrieving home directory");
        return None;
    };

    let mut path = dirs.config_dir().to_owned();
    path.push("config.kdl");
    Some(path)
}

fn system_config_path() -> PathBuf {
    PathBuf::from("/etc/niri/config.kdl")
}

fn config_path(cli_path: Option<PathBuf>) -> ConfigPath {
    if let Some(explicit) = cli_path.or_else(env_config_path) {
        return ConfigPath::Explicit(explicit);
    }

    let system_path = system_config_path();

    if let Some(user_path) = default_config_path() {
        ConfigPath::Regular {
            user_path,
            system_path,
        }
    } else {
        // Couldn't find the home directory, or whatever.
        ConfigPath::Explicit(system_path)
    }
}

fn notify_fd() -> anyhow::Result<()> {
    let fd = match env::var("NOTIFY_FD") {
        Ok(notify_fd) => notify_fd.parse()?,
        Err(env::VarError::NotPresent) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    env::remove_var("NOTIFY_FD");
    let mut notif = unsafe { File::from_raw_fd(fd) };
    notif.write_all(b"READY=1\n")?;
    Ok(())
}

// The wayland-server crate has set_default_max_buffer_size() under a libwayland_1_23 feature, but
// this hard-requires libwayland-server >= 1.23 which is not present on e.g. Ubuntu 24.04. Since
// calling this is an optional enhancement, do it optionally at runtime.
fn set_default_max_buffer_size(display: &Display<State>, size: usize) {
    use std::ffi::c_void;

    unsafe {
        // RTLD_NOLOAD ensures we only get a handle to the libwayland-server that wayland-rs has
        // already loaded into this process, rather than potentially pulling in a different copy.
        let lib = libc::dlopen(
            c"libwayland-server.so.0".as_ptr(),
            libc::RTLD_LAZY | libc::RTLD_NOLOAD,
        );
        if lib.is_null() {
            // It's not really expected that this can happen, maybe if some distro changes the
            // library name?
            warn!("cannot set default max buffer size: libwayland-server.so.0 is not loaded");
            return;
        }

        let sym = libc::dlsym(lib, c"wl_display_set_default_max_buffer_size".as_ptr());
        if sym.is_null() {
            // Expected on libwayland-server < 1.23.
            trace!("wl_display_set_default_max_buffer_size is missing; skipping");
        } else {
            let func: unsafe extern "C" fn(*mut c_void, libc::size_t) = std::mem::transmute(sym);
            let display_ptr = display.handle().backend_handle().display_ptr();
            func(display_ptr.cast(), size);
        }

        libc::dlclose(lib);
    }
}
