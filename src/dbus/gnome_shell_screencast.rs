//! `org.gnome.Shell.Screencast` — the high-level recorder entry point.
//!
//! In stock GNOME this name is owned by a separate gjs `dbusService` process that drives a
//! GStreamer pipeline over `org.gnome.Mutter.ScreenCast`. We can't rely on that helper being
//! present, so we own the name ourselves and back it with the compositor's native recorder
//! (`Synoik::start_native_recording`). The `<Ctrl><Shift><Alt>R` keybinding, the screenshot UI, and
//! any `gdbus` caller reach recording through here.
//!
//! Matches gnome-shell 50.1: methods `Screencast` (whole output) / `ScreencastArea` (a
//! global-logical rectangle) / `StopScreencast`, the `ScreencastSupported` property, options
//! `draw-cursor`(b)/`framerate`(i) (others ignored — we encode VP8/WebM), and gnome-shell's
//! file-template algorithm (`crate::recording`). Like the real service, a recording auto-stops when
//! the client that started it drops off the bus.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context as _;
use futures_util::StreamExt as _;
use zbus::fdo::{self, RequestNameFlags};
use zbus::message::Header;
use zbus::names::{OwnedUniqueName, UniqueName};
use zbus::zvariant::{NoneValue, OwnedValue};
use zbus::{interface, Task};

use super::Start;

/// A request from the D-Bus service to the compositor. Each carries a reply channel.
pub enum ScreencastToSynoik {
    Start {
        /// `Some((x, y, w, h))` for `ScreencastArea`; `None` for a full-output `Screencast`.
        area: Option<(i32, i32, i32, i32)>,
        /// The file template (gnome-shell semantics; see
        /// `crate::recording::resolve_file_template`).
        template: String,
        draw_cursor: bool,
        framerate: u32,
        /// `Ok(absolute_path)` on success, `Err(reason)` otherwise.
        reply: async_channel::Sender<Result<String, String>>,
    },
    Stop {
        /// Whether a recording was actually stopped.
        reply: async_channel::Sender<bool>,
    },
}

pub struct Screencast {
    to_niri: calloop::channel::Sender<ScreencastToSynoik>,
    /// Unique bus name of the client that started the active recording, shared with the monitor
    /// task so it can auto-stop when that client vanishes.
    owner: Arc<Mutex<Option<OwnedUniqueName>>>,
    monitor_task: Arc<OnceLock<Task<()>>>,
}

#[interface(name = "org.gnome.Shell.Screencast")]
impl Screencast {
    #[zbus(property)]
    async fn screencast_supported(&self) -> bool {
        true
    }

    async fn screencast(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        file_template: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(bool, String)> {
        self.start(hdr, None, file_template, options).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn screencast_area(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        file_template: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(bool, String)> {
        self.start(hdr, Some((x, y, width, height)), file_template, options)
            .await
    }

    async fn stop_screencast(&self) -> fdo::Result<bool> {
        *self.owner.lock().unwrap() = None;

        let (reply, rx) = async_channel::bounded(1);
        self.to_niri
            .send(ScreencastToSynoik::Stop { reply })
            .map_err(|_| fdo::Error::Failed("compositor is gone".to_owned()))?;
        rx.recv()
            .await
            .map_err(|_| fdo::Error::Failed("no reply from the compositor".to_owned()))
    }
}

impl Screencast {
    pub fn new(to_niri: calloop::channel::Sender<ScreencastToSynoik>) -> Self {
        Self {
            to_niri,
            owner: Arc::new(Mutex::new(None)),
            monitor_task: Arc::new(OnceLock::new()),
        }
    }

    async fn start(
        &self,
        hdr: Header<'_>,
        area: Option<(i32, i32, i32, i32)>,
        template: String,
        options: HashMap<String, OwnedValue>,
    ) -> fdo::Result<(bool, String)> {
        let draw_cursor = opt_bool(&options, "draw-cursor").unwrap_or(true);
        let framerate = opt_i32(&options, "framerate").unwrap_or(30).max(1) as u32;

        let (reply, rx) = async_channel::bounded(1);
        self.to_niri
            .send(ScreencastToSynoik::Start {
                area,
                template,
                draw_cursor,
                framerate,
                reply,
            })
            .map_err(|_| fdo::Error::Failed("compositor is gone".to_owned()))?;

        match rx.recv().await {
            Ok(Ok(path)) => {
                // Remember who started it, so the monitor task can auto-stop on disconnect.
                if let Some(sender) = hdr.sender() {
                    *self.owner.lock().unwrap() = Some(OwnedUniqueName::from(sender.to_owned()));
                }
                Ok((true, path))
            }
            // A request the compositor declined (already recording, area unsupported, no output):
            // report failure the gnome-shell way — `success = false`, empty filename.
            Ok(Err(reason)) => {
                warn!("screencast not started: {reason}");
                Ok((false, String::new()))
            }
            Err(_) => Err(fdo::Error::Failed(
                "no reply from the compositor".to_owned(),
            )),
        }
    }
}

fn opt_bool(options: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    options.get(key).and_then(|v| bool::try_from(v).ok())
}

fn opt_i32(options: &HashMap<String, OwnedValue>, key: &str) -> Option<i32> {
    options.get(key).and_then(|v| i32::try_from(v).ok())
}

/// Watch the bus; when the client that started the active recording disconnects, stop it — exactly
/// as gnome-shell's `_senderVanished` does. One long-lived task filters `NameOwnerChanged` for the
/// current owner.
async fn monitor_owner_disconnect(
    conn: &zbus::Connection,
    to_niri: calloop::channel::Sender<ScreencastToSynoik>,
    owner: Arc<Mutex<Option<OwnedUniqueName>>>,
) -> anyhow::Result<()> {
    let proxy = fdo::DBusProxy::new(conn)
        .await
        .context("error creating a DBusProxy")?;

    // Arg 2 is `new_owner`; a null value means the name was released (client gone).
    let mut stream = proxy
        .receive_name_owner_changed_with_args(&[(2, UniqueName::null_value())])
        .await
        .context("error creating a NameOwnerChanged stream")?;

    while let Some(signal) = stream.next().await {
        let args = signal
            .args()
            .context("error retrieving NameOwnerChanged args")?;

        let Some(name) = &**args.old_owner() else {
            continue;
        };

        let mut guard = owner.lock().unwrap();
        if guard.as_deref() == Some(name) {
            trace!("screencast owner {name} disconnected; stopping the recording");
            *guard = None;
            drop(guard);

            // Fire-and-forget stop; we don't need the reply.
            let (reply, _rx) = async_channel::bounded(1);
            let _ = to_niri.send(ScreencastToSynoik::Stop { reply });
        }
    }

    Ok(())
}

impl Start for Screencast {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let to_niri = self.to_niri.clone();
        let owner = self.owner.clone();
        let monitor_task = self.monitor_task.clone();

        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Shell/Screencast", self)?;
        conn.request_name_with_flags("org.gnome.Shell.Screencast", flags)?;

        let async_conn = conn.inner().clone();
        let future = {
            let conn = async_conn.clone();
            async move {
                if let Err(err) = monitor_owner_disconnect(&conn, to_niri, owner).await {
                    warn!("error monitoring screencast clients: {err:?}");
                }
            }
        };
        let task = async_conn
            .executor()
            .spawn(future, "monitor disappearing screencast clients");
        let _ = monitor_task.set(task);

        Ok(conn)
    }
}
