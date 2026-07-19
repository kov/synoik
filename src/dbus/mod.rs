use zbus::blocking::Connection;
use zbus::object_server::Interface;

use crate::niri::State;

pub mod freedesktop_a11y;
pub mod freedesktop_locale1;
pub mod freedesktop_login1;
pub mod freedesktop_screensaver;
pub mod gnome_session;
pub mod gnome_shell;
pub mod gnome_shell_introspect;
pub mod gnome_shell_screenshot;
pub mod mutter_display_config;
pub mod mutter_idle_monitor;
pub mod mutter_service_channel;
pub mod rfkill;
pub mod system_status;

#[cfg(feature = "xdp-gnome-screencast")]
pub mod gnome_shell_screencast;
#[cfg(feature = "xdp-gnome-screencast")]
pub mod mutter_screen_cast;
#[cfg(feature = "xdp-gnome-screencast")]
use mutter_screen_cast::ScreenCast;

use self::freedesktop_a11y::KeyboardMonitor;
use self::freedesktop_screensaver::ScreenSaver;
use self::gnome_session::EndSessionDialog;
use self::gnome_shell::GnomeShell;
use self::gnome_shell_introspect::Introspect;
use self::mutter_display_config::DisplayConfig;
use self::mutter_idle_monitor::IdleMonitor;
use self::mutter_service_channel::ServiceChannel;

trait Start: Interface {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection>;
}

#[derive(Default)]
pub struct DBusServers {
    pub conn_service_channel: Option<Connection>,
    pub conn_display_config: Option<Connection>,
    pub conn_screen_saver: Option<Connection>,
    pub conn_screen_shot: Option<Connection>,
    pub conn_introspect: Option<Connection>,
    pub conn_gnome_shell: Option<Connection>,
    pub conn_idle_monitor: Option<Connection>,
    pub conn_end_session: Option<Connection>,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub conn_screen_cast: Option<Connection>,
    #[cfg(feature = "xdp-gnome-screencast")]
    pub conn_shell_screencast: Option<Connection>,
    pub conn_login1: Option<Connection>,
    pub conn_locale1: Option<Connection>,
    pub conn_keyboard_monitor: Option<Connection>,
    pub conn_system_status: Option<Connection>,
    /// gsd-rfkill (session bus), for airplane mode. Kept for its watcher task *and* reused to
    /// write the `AirplaneMode` property when the QS toggle is clicked
    /// ([`rfkill::set_airplane_mode`]).
    pub conn_rfkill: Option<Connection>,
}

impl DBusServers {
    pub fn start(state: &mut State, is_session_instance: bool) {
        let _span = tracy_client::span!("DBusServers::start");

        let backend = &state.backend;
        let niri = &mut state.niri;
        let config = niri.config.borrow();

        let mut dbus = Self::default();

        if is_session_instance {
            let (to_niri, from_service_channel) = calloop::channel::channel();
            let service_channel = ServiceChannel::new(to_niri);
            niri.event_loop
                .insert_source(from_service_channel, move |event, _, state| match event {
                    calloop::channel::Event::Msg(new_client) => {
                        state.niri.insert_client(new_client);
                    }
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            dbus.conn_service_channel = try_start(service_channel);
        }

        if is_session_instance || config.debug.dbus_interfaces_in_non_session_instances {
            let (to_niri, from_display_config) = calloop::channel::channel();
            let display_config = DisplayConfig::new(to_niri, backend.ipc_outputs());
            niri.event_loop
                .insert_source(from_display_config, move |event, _, state| match event {
                    calloop::channel::Event::Msg(new_conf) => {
                        for (name, conf) in new_conf {
                            state.modify_output_config(&name, move |output| {
                                if let Some(new_output) = conf {
                                    *output = new_output;
                                } else {
                                    output.off = true;
                                }
                            });
                        }
                        state.reload_output_config();
                    }
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            dbus.conn_display_config = try_start(display_config);

            let screen_saver = ScreenSaver::new(niri.is_fdo_idle_inhibited.clone());
            dbus.conn_screen_saver = try_start(screen_saver);

            let (to_niri, from_screenshot) = calloop::channel::channel();
            let (to_screenshot, from_niri) = async_channel::unbounded();
            niri.event_loop
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
                niri.event_loop
                    .insert_source(from_screencast, move |event, _, state| match event {
                        calloop::channel::Event::Msg(msg) => {
                            state.niri.on_shell_screencast_msg(msg)
                        }
                        calloop::channel::Event::Closed => (),
                    })
                    .unwrap();
                let screencast = gnome_shell_screencast::Screencast::new(to_niri);
                dbus.conn_shell_screencast = try_start(screencast);
            }

            let (to_niri, from_introspect) = calloop::channel::channel();
            let (to_introspect, from_niri) = async_channel::unbounded();
            niri.event_loop
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
            niri.event_loop
                .insert_source(from_gnome_shell, move |event, _, state| match event {
                    calloop::channel::Event::Msg(msg) => state.on_gnome_shell_msg(msg),
                    calloop::channel::Event::Closed => (),
                })
                .unwrap();
            let gnome_shell = GnomeShell::new(to_niri);
            dbus.conn_gnome_shell = try_start(gnome_shell);

            let (to_niri, from_idle_monitor) = calloop::channel::channel();
            niri.event_loop
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
            niri.event_loop
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
                niri.event_loop
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
                niri.a11y_keyboard_monitor = Some(keyboard_monitor);
            }
        }

        let (to_niri, from_login1) = calloop::channel::channel();
        niri.event_loop
            .insert_source(from_login1, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_login1_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match freedesktop_login1::start(to_niri) {
            Ok(conn) => {
                dbus.conn_login1 = Some(conn);
            }
            Err(err) => {
                warn!("error starting login1 watcher: {err:?}");
            }
        }

        let (to_niri, from_locale1) = calloop::channel::channel();
        niri.event_loop
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
        niri.event_loop
            .insert_source(from_system_status, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_system_status_msg(msg),
                calloop::channel::Event::Closed => (),
            })
            .unwrap();
        match system_status::start(to_niri) {
            Ok(conn) => {
                dbus.conn_system_status = Some(conn);
            }
            Err(err) => {
                warn!("error starting system-status watcher: {err:?}");
            }
        }

        let (to_niri, from_rfkill) = calloop::channel::channel();
        niri.event_loop
            .insert_source(from_rfkill, move |event, _, state| match event {
                calloop::channel::Event::Msg(msg) => state.on_airplane_status(msg),
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

        niri.dbus = Some(dbus);
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
