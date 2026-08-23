// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The `org.freedesktop.Notifications` server.
//!
//! We own the well-known name and absorb both halves of gnome-shell 50.1's
//! split: the fdo proxy process (`js/dbusServices/notifications/notificationDaemon.js`
//! — sender PID resolution, per-sender id validation, unicast signals) and the
//! in-shell daemon (`js/ui/notificationDaemon.js` — hint parsing, source
//! keying). Like gnome-shell, the client's `expire_timeout` is ignored and the
//! `category`/`suppress-sound`/`x`/`y` hints are read by no one.
//!
//! Process-isolation seam: notifications are untrusted application content.
//! ALL untrusted parsing (hints, `image-data` pixel buffers, markup) happens
//! here, and the compositor is reached exclusively through the two plain-data
//! channels ([`NotificationsToSynoik`] in, [`SynoikToNotifications`] out) so this
//! module can be lifted into a separate process later. The main loop never
//! touches this connection — signal emission runs here, driven by
//! [`SynoikToNotifications`] commands, unicast to the notification's sender.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use futures_util::lock::Mutex;
use zbus::blocking::fdo::DBusProxy;
use zbus::fdo::{self, RequestNameFlags, RequestNameReply};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::{interface, zvariant};

use super::Start;
use crate::notifications::{
    bounded_pixels, clamp_text, flatten_text, sanitize_text, NotificationIcon,
    NotificationsToSynoik, NotifyRequest, PixelIcon, SynoikToNotifications, Urgency,
};

pub const PATH: &str = "/org/freedesktop/Notifications";
pub const BUS_NAME: &str = "org.freedesktop.Notifications";

type Hints = HashMap<String, zvariant::OwnedValue>;

pub struct Notifications {
    to_niri: calloop::channel::Sender<NotificationsToSynoik>,
    from_niri: async_channel::Receiver<SynoikToNotifications>,
    /// gnome-shell's daemon is single-threaded: `Notify`/`CloseNotification`
    /// are handled strictly in order. zbus dispatches method calls as
    /// concurrent tasks, so rapid same-sender calls (the `notify-send -r`
    /// replace pattern) could otherwise reorder around the PID-lookup await.
    serial: Mutex<()>,
}

impl Notifications {
    pub fn new(
        to_niri: calloop::channel::Sender<NotificationsToSynoik>,
        from_niri: async_channel::Receiver<SynoikToNotifications>,
    ) -> Self {
        Self {
            to_niri,
            from_niri,
            serial: Mutex::new(()),
        }
    }
}

fn sender(hdr: &Header<'_>) -> fdo::Result<String> {
    hdr.sender()
        .map(|name| name.to_string())
        .ok_or_else(|| fdo::Error::Failed("no sender".to_owned()))
}

fn internal_error(context: &str, err: impl std::fmt::Debug) -> fdo::Error {
    warn!("notifications: {context}: {err:?}");
    fdo::Error::Failed("internal error".to_owned())
}

fn hint_u32(hints: &Hints, key: &str) -> Option<u32> {
    let v = hints.get(key)?;
    if let Ok(v) = v.try_clone() {
        if let Ok(x) = u32::try_from(v) {
            return Some(x);
        }
    }
    if let Ok(v) = v.try_clone() {
        if let Ok(x) = u8::try_from(v) {
            return Some(u32::from(x));
        }
    }
    let v = v.try_clone().ok()?;
    i32::try_from(v).ok().and_then(|x| u32::try_from(x).ok())
}

fn hint_bool(hints: &Hints, key: &str) -> Option<bool> {
    let v = hints.get(key)?;
    if let Ok(v) = v.try_clone() {
        if let Ok(x) = bool::try_from(v) {
            return Some(x);
        }
    }
    hint_u32(hints, key).map(|x| x != 0)
}

fn hint_str(hints: &Hints, key: &str) -> Option<String> {
    let v = hints.get(key)?.try_clone().ok()?;
    String::try_from(v).ok()
}

/// Decode an `image-data`-style hint: the `(iiibiiay)` pixbuf tuple, converted
/// to a tight RGBA copy on this (untrusted-facing) side of the seam.
fn hint_pixels(hints: &Hints, key: &str) -> Option<Arc<PixelIcon>> {
    let v = hints.get(key)?.try_clone().ok()?;
    let (width, height, rowstride, has_alpha, bits_per_sample, channels, data) =
        <(i32, i32, i32, bool, i32, i32, Vec<u8>)>::try_from(v).ok()?;
    PixelIcon::from_wire(
        width,
        height,
        rowstride,
        has_alpha,
        bits_per_sample,
        channels,
        &data,
    )
    .map(bounded_pixels)
}

/// Decode a file-path icon (`image-path` / `app_icon` with a path or
/// `file://` URI) into bounded pixels HERE, on the untrusted side of the seam,
/// so the render path never opens client-named files. Undecodable → no icon.
fn load_file_icon(path: &std::path::Path) -> Option<Arc<PixelIcon>> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(bounded_pixels(PixelIcon {
        width,
        height,
        rgba: img.into_raw(),
    }))
}

/// Convert any `File` icon to bounded pixels; `Themed`/`Pixels` pass through.
pub(crate) fn resolve_file_icon(icon: NotificationIcon) -> Option<NotificationIcon> {
    match icon {
        NotificationIcon::File(path) => load_file_icon(&path).map(NotificationIcon::Pixels),
        other => Some(other),
    }
}

/// Decode in-band image bytes (an `org.gtk.Notifications` `("bytes", <ay>)`
/// serialized `GBytesIcon`, `glib/gio/gbytesicon.c`) into bounded pixels HERE,
/// on the untrusted side of the seam. Undecodable/oversized → no icon.
pub(crate) fn decode_bytes_icon(bytes: &[u8]) -> Option<NotificationIcon> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(NotificationIcon::Pixels(bounded_pixels(PixelIcon {
        width,
        height,
        rgba: img.into_raw(),
    })))
}

/// The notification's own icon from the image hints, with gnome-shell's exact
/// legacy fallback order (`js/ui/notificationDaemon.js:139-158,43-60`):
/// `image-data` → `image_data` → (`icon_data` only if no `image-path`/`image_path`),
/// else the `image-path`/`image_path` string.
fn notification_icon(hints: &Hints) -> Option<NotificationIcon> {
    let path = hint_str(hints, "image-path").or_else(|| hint_str(hints, "image_path"));
    let pixels = hint_pixels(hints, "image-data")
        .or_else(|| hint_pixels(hints, "image_data"))
        .or_else(|| {
            if path.is_none() {
                hint_pixels(hints, "icon_data")
            } else {
                None
            }
        });
    if let Some(pixels) = pixels {
        return Some(NotificationIcon::Pixels(pixels));
    }
    NotificationIcon::from_string(path.as_deref()?)
}

#[interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: Hints,
        _expire_timeout: i32,
    ) -> fdo::Result<u32> {
        let _guard = self.serial.lock().await;
        let sender = sender(&hdr)?;

        // gnome-shell's fdo proxy resolves the caller's PID for source keying
        // (`js/dbusServices/notifications/notificationDaemon.js:99-121`).
        let mut pid = 0;
        match fdo::DBusProxy::new(conn).await {
            Ok(proxy) => match BusName::try_from(sender.as_str()) {
                Ok(name) => match proxy.get_connection_unix_process_id(name).await {
                    Ok(x) => pid = x,
                    Err(err) => warn!("notifications: error resolving sender pid: {err:?}"),
                },
                Err(err) => warn!("notifications: bad sender name: {err:?}"),
            },
            Err(err) => warn!("notifications: error creating DBus proxy: {err:?}"),
        }

        // Action pairs; the special `default` id maps to body-click activation
        // instead of a button (`js/ui/notificationDaemon.js:218-241`). Keys and
        // labels are display/wire strings from an untrusted client: clamped,
        // labels flattened, and the pair count bounded (the UI shows at most 3;
        // a few spares survive for replaces).
        let mut has_default_action = false;
        let mut action_pairs = Vec::new();
        for pair in actions.as_chunks::<2>().0 {
            if pair[0] == "default" {
                has_default_action = true;
            } else if action_pairs.len() < 16 {
                action_pairs.push((
                    clamp_text(pair[0].clone(), 1024),
                    clamp_text(flatten_text(&pair[1]), 1024),
                ));
            }
        }

        let req = NotifyRequest {
            sender: Some(sender),
            pid,
            app_name: clamp_text(flatten_text(&app_name), 1024),
            replaces_id,
            desktop_entry: hint_str(&hints, "desktop-entry").map(|s| clamp_text(s, 255)),
            source_icon: NotificationIcon::from_string(&app_icon).and_then(resolve_file_icon),
            // Resolved by the compositor, which owns the app catalog (`on_notifications_msg`).
            app_icon: None,
            // The summary displays verbatim in gnome-shell (escaped wholesale,
            // `js/ui/messageList.js:564-568`); only the body is markup-capable.
            title: clamp_text(flatten_text(&summary), 4096),
            body: clamp_text(sanitize_text(&body), 8192),
            icon: notification_icon(&hints).and_then(resolve_file_icon),
            actions: action_pairs,
            has_default_action,
            // Absent urgency defaults to normal (`js/ui/notificationDaemon.js:144`).
            urgency: Urgency::from_wire(hint_u32(&hints, "urgency").unwrap_or(1)),
            resident: hint_bool(&hints, "resident").unwrap_or(false),
            transient: hint_bool(&hints, "transient").unwrap_or(false),
        };

        let (reply, rx) = async_channel::bounded(1);
        self.to_niri
            .send(NotificationsToSynoik::Notify { req, reply })
            .map_err(|err| internal_error("error sending message to synoik", err))?;
        rx.recv()
            .await
            .map_err(|err| internal_error("error receiving message from synoik", err))?
            .map_err(|_| fdo::Error::InvalidArgs("Invalid notification ID".to_owned()))
    }

    async fn close_notification(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        id: u32,
    ) -> fdo::Result<()> {
        let _guard = self.serial.lock().await;
        let sender = sender(&hdr)?;
        let (reply, rx) = async_channel::bounded(1);
        self.to_niri
            .send(NotificationsToSynoik::Close { id, sender, reply })
            .map_err(|err| internal_error("error sending message to synoik", err))?;
        rx.recv()
            .await
            .map_err(|err| internal_error("error receiving message from synoik", err))?
            .map_err(|_| fdo::Error::InvalidArgs("Invalid notification ID".to_owned()))
    }

    /// gnome-shell's list (`js/ui/notificationDaemon.js:276-289`) minus
    /// `sound`, which we don't play (clients then play their own).
    async fn get_capabilities(&self) -> Vec<String> {
        [
            "actions",
            "body",
            "body-markup",
            "icon-static",
            "persistence",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "synoik".to_owned(),
            "GNOME".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
            "1.2".to_owned(),
        )
    }

    // The three signals are emitted unicast to the posting client by the
    // emitter task in `start` (never broadcast); these declarations provide
    // the introspection XML.
    #[zbus(signal)]
    pub async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn activation_token(
        emitter: &SignalEmitter<'_>,
        id: u32,
        activation_token: String,
    ) -> zbus::Result<()>;
}

impl Start for Notifications {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let to_niri = self.to_niri.clone();
        let from_niri = self.from_niri.clone();
        let conn = zbus::blocking::Connection::session()?;

        conn.object_server().at(PATH, self)?;

        // No AllowReplacement: a notification daemon started later must not
        // silently steal the name from us (gnome-shell's proxy likewise owns
        // with REPLACE only). And zbus reports an already-owned name as
        // `Ok(Exists)`, not an error — treat anything but ownership as a
        // failed start so we never emit signals from a connection that does
        // not own the name.
        let flags = RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue;
        match conn.request_name_with_flags(BUS_NAME, flags)? {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => (),
            reply => anyhow::bail!(
                "{BUS_NAME} is owned by another notification daemon \
                 (request replied {reply:?}); notifications server disabled"
            ),
        }

        // Emitter task: unicast signal emission driven by the main loop's
        // commands. Skips notifications whose sender is unknown, like
        // gnome-shell's proxy (`js/dbusServices/notifications/notificationDaemon.js:56-65`).
        let emit_conn = conn.inner().clone();
        conn.inner()
            .executor()
            .spawn(
                async move {
                    while let Ok(msg) = from_niri.recv().await {
                        match msg {
                            SynoikToNotifications::Closed { id, reason, sender } => {
                                let Some(sender) = sender else { continue };
                                let Ok(dest) = BusName::try_from(sender.as_str()) else {
                                    continue;
                                };
                                if let Err(err) = emit_conn
                                    .emit_signal(
                                        Some(dest),
                                        PATH,
                                        BUS_NAME,
                                        "NotificationClosed",
                                        &(id, reason.wire_code()),
                                    )
                                    .await
                                {
                                    warn!(
                                        "notifications: error emitting NotificationClosed: \
                                         {err:?}"
                                    );
                                }
                            }
                            SynoikToNotifications::ActionInvoked {
                                id,
                                action,
                                token,
                                sender,
                            } => {
                                let Some(sender) = sender else { continue };
                                let Ok(dest) = BusName::try_from(sender.as_str()) else {
                                    continue;
                                };
                                // ActivationToken always immediately precedes
                                // ActionInvoked (`js/ui/notificationDaemon.js:224-236`).
                                if let Err(err) = emit_conn
                                    .emit_signal(
                                        Some(dest.clone()),
                                        PATH,
                                        BUS_NAME,
                                        "ActivationToken",
                                        &(id, token),
                                    )
                                    .await
                                {
                                    warn!("notifications: error emitting ActivationToken: {err:?}");
                                }
                                if let Err(err) = emit_conn
                                    .emit_signal(
                                        Some(dest),
                                        PATH,
                                        BUS_NAME,
                                        "ActionInvoked",
                                        &(id, action),
                                    )
                                    .await
                                {
                                    warn!("notifications: error emitting ActionInvoked: {err:?}");
                                }
                            }
                        }
                    }
                },
                "notifications signal emitter",
            )
            .detach();

        // Tear down app sources whose sender left the bus, like gnome-shell's
        // per-sender bus-name watch (`js/ui/notificationDaemon.js:330-348`).
        let watch_conn = conn.clone();
        thread::Builder::new()
            .name("org.freedesktop.Notifications name watcher".to_owned())
            .spawn(move || {
                let proxy = match DBusProxy::new(&watch_conn) {
                    Ok(proxy) => proxy,
                    Err(err) => {
                        warn!("error creating DBus proxy: {err:?}");
                        return;
                    }
                };
                let changed = match proxy.receive_name_owner_changed() {
                    Ok(changed) => changed,
                    Err(err) => {
                        warn!("error subscribing to NameOwnerChanged: {err:?}");
                        return;
                    }
                };
                for signal in changed {
                    let Ok(args) = signal.args() else { continue };
                    if let (BusName::Unique(name), None) = (&args.name, args.new_owner.as_ref()) {
                        if to_niri
                            .send(NotificationsToSynoik::SenderVanished(name.to_string()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            })?;

        Ok(conn)
    }
}
