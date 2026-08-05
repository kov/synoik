// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use zbus::blocking::Connection;
use zbus::object_server::Interface;

use crate::synoik::State;

pub mod accounts_service;
pub mod bluez;
pub mod calendar_server;
pub mod dbusmenu;
pub mod fprintd;
pub mod freedesktop_a11y;
pub mod freedesktop_locale1;
pub mod freedesktop_login1;
pub mod freedesktop_notifications;
pub mod freedesktop_screensaver;
pub mod gdm;
pub mod gnome_screen_saver;
pub mod gnome_session;
pub mod gnome_session_presence;
pub mod gnome_shell;
pub mod gnome_shell_brightness;
pub mod gnome_shell_introspect;
pub mod gnome_shell_screenshot;
pub mod gtk_notifications;
pub mod mpris;
pub mod mutter_display_config;
pub mod mutter_idle_monitor;
pub mod mutter_service_channel;
pub mod polkit_agent;
pub mod rfkill;
pub mod system_status;
pub mod user_switching;

#[cfg(feature = "xdp-gnome-screencast")]
pub mod gnome_shell_screencast;
#[cfg(feature = "xdp-gnome-screencast")]
pub mod mutter_screen_cast;
pub mod smartcard;
pub mod status_notifier;
#[cfg(feature = "xdp-gnome-screencast")]
use mutter_screen_cast::ScreenCast;

use self::freedesktop_a11y::KeyboardMonitor;
use self::freedesktop_notifications::Notifications;
use self::freedesktop_screensaver::ScreenSaver;
use self::gnome_screen_saver::GnomeScreenSaver;
use self::gnome_session::EndSessionDialog;
use self::gnome_shell::GnomeShell;
use self::gnome_shell_brightness::Brightness;
use self::gnome_shell_introspect::Introspect;
use self::gtk_notifications::GtkNotifications;
use self::mutter_display_config::DisplayConfig;
use self::mutter_idle_monitor::IdleMonitor;
use self::mutter_service_channel::ServiceChannel;
use self::status_notifier::StatusNotifierWatcher;

trait Start: Interface {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection>;
}

#[derive(Default)]
pub struct DBusServers {
    pub conn_service_channel: Option<Connection>,
    pub conn_display_config: Option<Connection>,
    pub conn_screen_saver: Option<Connection>,
    /// The system-bus connection behind the unlock dialog's verifier.
    pub conn_gdm: Option<Connection>,
    /// org.gnome.ScreenSaver + org.gnome.Shell.ScreenShield — the *locking* screensaver
    /// interface, as opposed to `conn_screen_saver`'s inhibit-only one.
    pub conn_screen_shield: Option<Connection>,
    pub conn_screen_shot: Option<Connection>,
    pub conn_introspect: Option<Connection>,
    pub conn_gnome_shell: Option<Connection>,
    pub conn_idle_monitor: Option<Connection>,
    pub conn_end_session: Option<Connection>,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub conn_screen_cast: Option<Connection>,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub conn_shell_screencast: Option<Connection>,
    pub conn_notifications: Option<Connection>,
    pub conn_gtk_notifications: Option<Connection>,
    pub conn_login1: Option<Connection>,
    /// gnome-session's presence, the shield's idle source.
    pub conn_presence: Option<Connection>,
    pub conn_accounts: Option<Connection>,
    pub conn_fprintd: Option<Connection>,
    pub conn_smartcard: Option<Connection>,
    /// The session's polkit authentication agent (system bus). Without it, every action needing
    /// authentication fails with no prompt — see [`polkit_agent`].
    pub conn_polkit: Option<Connection>,
    pub conn_locale1: Option<Connection>,
    pub conn_keyboard_monitor: Option<Connection>,
    pub conn_system_status: Option<Connection>,
    /// gsd-rfkill (session bus), for airplane mode. Kept for its watcher task *and* reused to
    /// write the `AirplaneMode` property when the QS toggle is clicked
    /// ([`rfkill::set_airplane_mode`]).
    pub conn_rfkill: Option<Connection>,
    /// org.gnome.Shell.CalendarServer (session bus) — the dateMenu Events source.
    pub conn_calendar_server: Option<Connection>,
    /// The MPRIS watcher (session bus): every `org.mpris.MediaPlayer2.*` player, plus the
    /// connection its controls are called on.
    pub conn_mpris: Option<Connection>,
    /// org.kde.StatusNotifierWatcher (session bus) — app-indicator support. Owning this name is
    /// itself the feature: clients hide their tray affordance when nothing owns it.
    pub conn_status_notifier: Option<Connection>,
    /// org.gnome.Shell.Brightness (session bus) — gsd-power's way in to idle dimming and the
    /// auto-brightness target. Its own well-known name, hence its own connection.
    pub conn_brightness: Option<Connection>,
}

impl DBusServers {
    pub fn start(state: &mut State, is_session_instance: bool) {
        let _span = tracy_client::span!("DBusServers::start");

        let backend = &state.backend;
        let synoik = &mut state.synoik;
        let config = synoik.config.borrow();

        let mut dbus = Self::default();

        if is_session_instance {
            let (to_niri, from_service_channel) = calloop::channel::channel();
            let service_channel = ServiceChannel::new(to_niri);
            synoik
                .event_loop
                .insert_source(from_service_channel, move |event, _, state| match event {
                    calloop::channel::Event::Msg(new_client) => {
                        state.synoik.insert_client(new_client);
                    }
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            dbus.conn_service_channel = try_start(service_channel);
        }

        if is_session_instance || config.debug.dbus_interfaces_in_non_session_instances {
            let (to_niri, from_display_config) = calloop::channel::channel();
            let display_config = DisplayConfig::new(to_niri, backend.ipc_outputs());
            synoik
                .event_loop
                .insert_source(from_display_config, move |event, _, state| match event {
                    calloop::channel::Event::Msg(new_conf) => {
                        state.apply_display_config(new_conf);
                    }
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            dbus.conn_display_config = try_start(display_config);

            let screen_saver = ScreenSaver::new(synoik.is_fdo_idle_inhibited.clone());
            dbus.conn_screen_saver = try_start(screen_saver);

            // The lock half. A separate name and a separate object from the inhibit-only
            // `org.freedesktop.ScreenSaver` above; see `dbus::gnome_screen_saver`.
            let (to_niri, from_screen_saver) = calloop::channel::channel();
            let (to_screen_saver, from_niri) = async_channel::unbounded();
            synoik
                .event_loop
                .insert_source(from_screen_saver, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_screen_saver_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let shield = GnomeScreenSaver::new(to_niri, from_niri, synoik.shield_snapshot.clone());
            if let Some(conn) = try_start(shield) {
                dbus.conn_screen_shield = Some(conn);
                synoik.screen_saver_emit = Some(to_screen_saver);
            }

            // The verifier behind the unlock dialog. This is what decides whether the shield may
            // lock at all — a session where it fails to start gets a screensaver, not a lock.
            let (to_niri, from_gdm) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_gdm, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_verifier_event(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            match gdm::start(to_niri) {
                Ok((conn, requests)) => {
                    dbus.conn_gdm = Some(conn);
                    synoik.gdm_requests = Some(requests);
                }
                Err(err) => warn!("error starting the gdm verifier client: {err:?}"),
            }

            // gsd-power's way in to brightness: idle dimming and the auto-brightness target.
            let (to_niri, from_brightness) = calloop::channel::channel();
            let (to_brightness, from_niri) = async_channel::unbounded();
            synoik
                .event_loop
                .insert_source(from_brightness, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_brightness_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let brightness = Brightness::new(to_niri, from_niri);
            if let Some(conn) = try_start(brightness) {
                dbus.conn_brightness = Some(conn);
                synoik.brightness_emit = Some(to_brightness);
            }

            let (to_niri, from_screenshot) = calloop::channel::channel();
            let (to_screenshot, from_niri) = async_channel::unbounded();
            synoik
                .event_loop
                .insert_source(from_screenshot, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => {
                        state.on_screen_shot_msg(&to_screenshot, msg)
                    }
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let screenshot = gnome_shell_screenshot::Screenshot::new(to_niri, from_niri);
            dbus.conn_screen_shot = try_start(screenshot);

            #[cfg(feature = "xdp-gnome-screencast")]
            {
                let (to_niri, from_screencast) = calloop::channel::channel();
                synoik
                    .event_loop
                    .insert_source(from_screencast, move |event, _, state| match event {
                        calloop::channel::Event::Msg(msg) => {
                            state.synoik.on_shell_screencast_msg(msg)
                        }
                        calloop::channel::Event::Closed => (),
                    })
                    .unwrap();
                let screencast = gnome_shell_screencast::Screencast::new(to_niri);
                dbus.conn_shell_screencast = try_start(screencast);
            }

            let (to_niri, from_introspect) = calloop::channel::channel();
            let (to_introspect, from_niri) = async_channel::unbounded();
            synoik
                .event_loop
                .insert_source(from_introspect, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => {
                        state.on_introspect_msg(&to_introspect, msg)
                    }
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let introspect = Introspect::new(to_niri, from_niri);
            dbus.conn_introspect = try_start(introspect);

            let (to_niri, from_gnome_shell) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_gnome_shell, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_gnome_shell_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let gnome_shell = GnomeShell::new(to_niri);
            dbus.conn_gnome_shell = try_start(gnome_shell);

            let (to_niri, from_idle_monitor) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_idle_monitor, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_idle_monitor_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let idle_monitor = IdleMonitor::new(to_niri);
            dbus.conn_idle_monitor = try_start(idle_monitor);

            // gnome-session calls `EndSessionDialog.Open` on the `org.gnome.Shell` bus name, so the
            // object must live on that same connection (gnome-shell likewise exports it on its own
            // session connection), not a separate one — otherwise the Open lands as UnknownObject.
            let (to_niri, from_end_session) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_end_session, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_end_session_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            if let Some(conn) = &dbus.conn_gnome_shell {
                let end_session = EndSessionDialog::new(to_niri);
                match conn
                    .object_server()
                    .at("/org/gnome/SessionManager/EndSessionDialog", end_session)
                {
                    Ok(_) => dbus.conn_end_session = Some(conn.clone()),
                    Err(err) => warn!("error exporting EndSessionDialog: {err:?}"),
                }
            }

            #[cfg(feature = "xdp-gnome-screencast")]
            {
                let (to_niri, from_screen_cast) = calloop::channel::channel();
                synoik
                    .event_loop
                    .insert_source(from_screen_cast, {
                        move |event, _, state| match event {
                            calloop::channel::Event::Msg(msg) => state.on_screen_cast_msg(msg),
                            calloop::channel::Event::Closed => (),
                        }
                    })
                    .unwrap();
                let screen_cast = ScreenCast::new(backend.ipc_outputs(), to_niri);
                dbus.conn_screen_cast = try_start(screen_cast);
            }

            let keyboard_monitor = KeyboardMonitor::new();
            if let Some(x) = try_start(keyboard_monitor.clone()) {
                dbus.conn_keyboard_monitor = Some(x);
                synoik.a11y_keyboard_monitor = Some(keyboard_monitor);
            }

            let (to_niri, from_notifications) = calloop::channel::channel();
            let (to_notifications, from_niri) = async_channel::unbounded();
            synoik
                .event_loop
                .insert_source(from_notifications, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_notifications_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let notifications = Notifications::new(to_niri.clone(), from_niri);
            if let Some(conn) = try_start(notifications) {
                dbus.conn_notifications = Some(conn);
                synoik.notifications_emit = Some(to_notifications);
            }

            // The Gtk daemon shares the inbound channel (both front-ends feed
            // the one store via `on_notifications_msg`) but owns a separate
            // outbound channel: its `ActionInvoked` signal differs in shape and
            // is broadcast (`js/ui/notificationDaemon.js:508-534`).
            let (to_gtk, gtk_from_niri) = async_channel::unbounded();
            let gtk_notifications = GtkNotifications::new(to_niri, gtk_from_niri);
            if let Some(conn) = try_start(gtk_notifications) {
                dbus.conn_gtk_notifications = Some(conn);
                synoik.gtk_notifications_emit = Some(to_gtk);
            }

            // App indicators. GNOME has no equivalent — see `docs/fork/status-notifier-port.md`.
            let (to_niri, from_status_notifier) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_status_notifier, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_status_notifier_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let (to_status_notifier, sn_from_niri) = async_channel::unbounded();
            dbus.conn_status_notifier =
                try_start(StatusNotifierWatcher::new(to_niri, sn_from_niri));
            if dbus.conn_status_notifier.is_some() {
                synoik.status_notifier_emit = Some(to_status_notifier);
            }
        }

        let (to_niri, from_login1) = calloop::channel::channel();
        synoik
            .event_loop
            .insert_source(from_login1, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_login1_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match freedesktop_login1::start(to_niri.clone()) {
            Ok(conn) => {
                dbus.conn_login1 = Some(conn);
                // Kept for the backlight write path, which needs to report completions back.
                synoik.login1_tx = Some(to_niri);
            }
            Err(err) => {
                warn!("error starting login1 watcher: {err:?}");
            }
        }

        // The polkit authentication agent.
        //
        // **After login1**, which is what resolves our session path — the subject we register for
        // is our logind session, and there is no other way to name it. Started before that, the
        // agent silently never registers and every polkit action in the session fails with no
        // prompt, which is the exact bug it exists to fix.
        //
        // On the same `is_session_instance` gate as the rest: registration is per-session and
        // polkitd allows one agent per session, so a second instance would take the seat's prompt
        // away from the real shell.
        if is_session_instance {
            let (to_niri, from_polkit) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_polkit, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_polkit_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            match polkit_agent::start(to_niri) {
                Ok((conn, requests)) => {
                    dbus.conn_polkit = Some(conn);
                    synoik.polkit_requests = Some(requests);
                }
                // Loud: the failure mode is silent everywhere else. Every polkit action will fail
                // with no prompt, and the user has no way to tell that from being denied.
                Err(err) => warn!("no polkit authentication agent: {err:?}"),
            }
        }

        let (to_niri, from_presence) = calloop::channel::channel();
        synoik
            .event_loop
            .insert_source(from_presence, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_presence_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match gnome_session_presence::start(to_niri) {
            Ok(conn) => dbus.conn_presence = Some(conn),
            Err(err) => warn!("error starting gnome-session presence watcher: {err:?}"),
        }

        let (to_niri, from_accounts) = calloop::channel::channel();
        synoik
            .event_loop
            .insert_source(from_accounts, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_accounts_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match accounts_service::start(synoik.unlock_dialog.user().name.clone(), to_niri) {
            Ok(conn) => dbus.conn_accounts = Some(conn),
            Err(err) => warn!("error starting AccountsService watcher: {err:?}"),
        }

        // Only if the user has not turned fingerprint authentication off — the probe can activate
        // fprintd, and activating a service the user has declined is not ours to do.
        if synoik.gnome_settings.shield.enable_fingerprint {
            let (to_niri, from_fprintd) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_fprintd, move |event, _, state| match event {
                    calloop::channel::Event::Msg(reader) => state.on_fingerprint_reader(reader),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            match fprintd::start(to_niri) {
                Ok(conn) => dbus.conn_fprintd = Some(conn),
                Err(err) => warn!("error probing for a fingerprint reader: {err:?}"),
            }
        }

        // Smartcards, gated the same way — and unlike fprintd this cannot activate anything: gsd's
        // smartcard plugin is either running in the session or it is not.
        {
            let enabled = synoik.gnome_settings.shield.enable_smartcard;
            let (to_niri, from_smartcard) = calloop::channel::channel();
            synoik
                .event_loop
                .insert_source(from_smartcard, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_smartcard_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            match smartcard::start(enabled, to_niri) {
                Ok(conn) => dbus.conn_smartcard = Some(conn),
                Err(err) => warn!("error watching for smartcards: {err:?}"),
            }
        }

        let (to_niri, from_locale1) = calloop::channel::channel();
        synoik
            .event_loop
            .insert_source(from_locale1, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_locale1_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match freedesktop_locale1::start(to_niri) {
            Ok(conn) => {
                dbus.conn_locale1 = Some(conn);
            }
            Err(err) => {
                warn!("error starting locale1 watcher: {err:?}");
            }
        }

        let (to_niri, from_system_status) = calloop::channel::channel();
        synoik
            .event_loop
            .insert_source(from_system_status, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_system_status_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        // The bluez connect/disconnect writer reports completion back through this same channel.
        synoik.system_status_tx = Some(to_niri.clone());
        match system_status::start(to_niri) {
            Ok(conn) => {
                dbus.conn_system_status = Some(conn);
            }
            Err(err) => {
                warn!("error starting system-status watcher: {err:?}");
            }
        }

        let (to_niri, from_rfkill) = calloop::channel::channel();
        synoik
            .event_loop
            .insert_source(from_rfkill, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_rfkill_status(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match rfkill::start(to_niri) {
            Ok(conn) => {
                dbus.conn_rfkill = Some(conn);
            }
            Err(err) => {
                warn!("error starting rfkill watcher: {err:?}");
            }
        }

        // Calendar events (org.gnome.Shell.CalendarServer): bidirectional — we
        // push the visible day range out and receive event signals back.
        let (to_niri, from_calendar) = calloop::channel::channel();
        let (to_calendar, calendar_from_niri) = async_channel::unbounded();
        synoik
            .event_loop
            .insert_source(from_calendar, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_calendar_events_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match calendar_server::start(to_niri, calendar_from_niri) {
            Ok(conn) => {
                dbus.conn_calendar_server = Some(conn);
                synoik.calendar_range_emit = Some(to_calendar);
                // Request today's month grid so the service activates and events
                // are ready before the popover first opens.
                synoik.sync_calendar_range();
            }
            Err(err) => {
                warn!("error starting calendar-server watcher: {err:?}");
            }
        }

        // MPRIS media players: their state comes in, the card's controls go out.
        let (to_niri, from_mpris) = calloop::channel::channel();
        let (to_mpris, mpris_from_niri) = async_channel::unbounded();
        synoik
            .event_loop
            .insert_source(from_mpris, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_mpris_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match mpris::start(to_niri, mpris_from_niri) {
            Ok(conn) => {
                dbus.conn_mpris = Some(conn);
                synoik.mpris_emit = Some(to_mpris);
            }
            Err(err) => {
                warn!("error starting MPRIS watcher: {err:?}");
            }
        }

        synoik.dbus = Some(dbus);
    }
}

fn try_start<I: Start>(iface: I) -> Option<Connection> {
    match iface.start() {
        Ok(conn) => Some(conn),
        Err(err) => {
            warn!("error starting {}: {err:?}", I::name());
            None
        }
    }
}

/// Ask GNOME Software to show `app_id`'s page — the "App Details" menu row
/// (`js/ui/appMenu.js:84-95`), which activates Software's `details` action over
/// `org.gtk.Actions` rather than launching it with an argument.
///
/// Runs on its own thread: the call D-Bus *activates* Software, so waiting for the
/// reply on the compositor thread would stall the frame loop for as long as it takes
/// to start. gnome-shell's handler is `async` for the same reason. The empty second
/// element of the parameter is gnome-shell's — it is the search term to preselect.
pub fn show_app_details(app_id: String) {
    std::thread::spawn(move || {
        if let Err(err) = call_app_details(app_id) {
            warn!("error asking Software for app details: {err:?}");
        }
    });
}

fn call_app_details(app_id: String) -> zbus::Result<()> {
    let conn = Connection::session()?;
    let args = zbus::zvariant::Value::from((app_id, String::new()));
    conn.call_method(
        Some("org.gnome.Software"),
        "/org/gnome/Software",
        Some("org.gtk.Actions"),
        "Activate",
        &(
            "details",
            vec![args],
            std::collections::HashMap::<String, zbus::zvariant::Value<'_>>::new(),
        ),
    )?;
    Ok(())
}

/// Reject a D-Bus caller that is not on `allowlist` — GNOME's `DBusSenderChecker`
/// (`js/misc/util.js:344-409`).
///
/// Several of the shell's interfaces hand out things the user would not want handed out on request
/// — the window list, a picture of the screen — and GNOME gates each on a *different* short list of
/// well-known names. So the list belongs to the caller, but the check does not: it is the same
/// resolve-and-compare every time, and a second copy of it is a second place to get it subtly
/// wrong.
///
/// The owners are resolved per call rather than cached from a `watch_name`. These calls happen when
/// a portal dialog opens, so the round trips cost nothing anyone can perceive, and a cache that
/// goes stale in the permissive direction is exactly the hole the list exists to close.
///
/// GNOME also skips the whole check under `global.context.unsafe_mode`. We have no unsafe mode and
/// are not adding one for this.
pub async fn check_sender(
    conn: &zbus::Connection,
    sender: Option<&zbus::names::UniqueName<'_>>,
    allowlist: &[&str],
    method: &str,
) -> zbus::fdo::Result<()> {
    let denied = || zbus::fdo::Error::AccessDenied(format!("{method} is not allowed"));

    // No sender means no way to tell who is asking, which is not a reason to answer.
    let Some(sender) = sender else {
        return Err(denied());
    };

    let proxy = zbus::fdo::DBusProxy::new(conn)
        .await
        .map_err(|err| zbus::fdo::Error::Failed(format!("{err}")))?;

    for name in allowlist {
        let Ok(name) = zbus::names::BusName::try_from(*name) else {
            continue;
        };
        // An allowlisted name with no owner simply does not match: that peer is not running, so
        // nothing it would have been allowed to ask is being asked.
        if let Ok(owner) = proxy.get_name_owner(name).await {
            if owner.as_str() == sender.as_str() {
                return Ok(());
            }
        }
    }

    warn!("{method} refused for {sender}: not an allowed caller");
    Err(denied())
}
