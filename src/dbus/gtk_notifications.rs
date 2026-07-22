//! The `org.gtk.Notifications` server (GNOME's `GtkNotificationDaemon`,
//! `js/ui/notificationDaemon.js:581-712`).
//!
//! `GNotification`/`g_application_send_notification` clients (GTK/GNOME apps)
//! post here; GLib apps fall back to `org.freedesktop.Notifications` only when
//! this name is unowned. Unlike the fdo path the payload carries no app name or
//! icon: the source's identity comes from the caller's installed
//! `${app_id}.desktop` (gnome-shell's `Shell.AppSystem.lookup_app`,
//! `js/ui/notificationDaemon.js:493-501`), and an app-id with no desktop file
//! is rejected with `org.gtk.Notifications.Error.InvalidApp`.
//!
//! Both notification front-ends feed the one [`NotificationStore`] over the
//! same inbound channel; this server owns a *separate* outbound channel
//! ([`GtkToNotifications`]) because the Gtk `ActionInvoked` signal has a
//! different signature and is broadcast, and `app.`-prefixed actions route to
//! the app itself instead of a signal (deferred to slice 2).
//!
//! Process-isolation seam: as with the fdo server, ALL untrusted parsing (the
//! `a{sv}` payload, the serialized `GIcon`, in-band image bytes, the desktop
//! file) happens here; the compositor is reached only through plain-data
//! channels.
//!
//! [`NotificationStore`]: crate::notifications::NotificationStore

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use futures_util::lock::Mutex;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::{interface, zvariant};

use super::freedesktop_notifications::{decode_bytes_icon, resolve_file_icon};
use super::Start;
use crate::notifications::{
    clamp_text, flatten_text, sanitize_text, GtkNotifyRequest, GtkToNotifications,
    NotificationIcon, NotificationsToNiri, Urgency,
};

pub const PATH: &str = "/org/gtk/Notifications";
pub const BUS_NAME: &str = "org.gtk.Notifications";

type Dict = HashMap<String, zvariant::OwnedValue>;

/// The `org.gtk.Notifications.Error.InvalidApp` D-Bus error (GNOME registers
/// this domain in `js/misc/dbusErrors.js:32-36`), returned when the app-id has
/// no installed desktop file.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.gtk.Notifications.Error")]
enum GtkError {
    #[zbus(error)]
    ZBus(zbus::Error),
    InvalidApp(String),
}

pub struct GtkNotifications {
    to_niri: calloop::channel::Sender<NotificationsToNiri>,
    from_niri: async_channel::Receiver<GtkToNotifications>,
    /// Serialize method calls, like the fdo server: zbus dispatches concurrent
    /// tasks, but `Add`/`Remove` for one `(app_id, id)` must stay ordered.
    serial: Mutex<()>,
}

impl GtkNotifications {
    pub fn new(
        to_niri: calloop::channel::Sender<NotificationsToNiri>,
        from_niri: async_channel::Receiver<GtkToNotifications>,
    ) -> Self {
        Self {
            to_niri,
            from_niri,
            serial: Mutex::new(()),
        }
    }
}

/// The display identity gnome-shell pulls from the app's desktop file.
struct AppInfo {
    /// The `Name` key (absent → the caller falls back to the app-id).
    name: Option<String>,
    /// The `Icon` key, resolved to bounded pixels or a themed name.
    icon: Option<NotificationIcon>,
}

/// A subset of `g_application_id_is_valid` (`glib/gio/gapplication.c`) that also
/// bars anything that could walk the desktop-file lookup out of its
/// directories: ASCII `[A-Za-z0-9-_.]`, 1..=255 bytes, at least one `.`, no
/// leading/trailing `.` and no `..`.
fn app_id_is_valid(app_id: &str) -> bool {
    if app_id.is_empty() || app_id.len() > 255 {
        return false;
    }
    if !app_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return false;
    }
    if app_id.starts_with('.') || app_id.ends_with('.') || app_id.contains("..") {
        return false;
    }
    app_id.contains('.')
}

/// `$XDG_DATA_HOME/applications` then each `$XDG_DATA_DIRS/applications`, in the
/// XDG precedence order gnome-shell's AppSystem searches.
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
        Some(home) => dirs.push(PathBuf::from(home).join("applications")),
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share/applications"));
            }
        }
    }
    let data_dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
    for dir in std::env::split_paths(&data_dirs) {
        dirs.push(dir.join("applications"));
    }
    dirs
}

/// Read the `[Desktop Entry]` `Name`/`Icon` from a desktop file. Only the
/// unlocalized keys are read (gnome-shell uses the localized name — a recorded
/// divergence); the icon string is resolved through the same bounded loader as
/// the fdo path, so a file icon is decoded on this untrusted side of the seam.
fn read_desktop_entry(path: &Path) -> Option<AppInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut name = None;
    let mut icon = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(group) = line.strip_prefix('[') {
            in_entry = group.strip_suffix(']') == Some("Desktop Entry");
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Name" if name.is_none() => name = Some(value.trim().to_owned()),
            "Icon" if icon.is_none() => icon = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    Some(AppInfo {
        name,
        icon: icon
            .as_deref()
            .and_then(NotificationIcon::from_string)
            .and_then(resolve_file_icon),
    })
}

/// Resolve `${app_id}.desktop`; `None` (→ `InvalidApp`) when the app-id is
/// malformed or no desktop file is installed for it.
fn resolve_app(app_id: &str) -> Option<AppInfo> {
    if !app_id_is_valid(app_id) {
        return None;
    }
    let filename = format!("{app_id}.desktop");
    application_dirs()
        .into_iter()
        .find_map(|dir| read_desktop_entry(&dir.join(&filename)))
}

fn dict_str(dict: &Dict, key: &str) -> Option<String> {
    let value = dict.get(key)?.try_clone().ok()?;
    String::try_from(value).ok()
}

/// Decode a serialized `GIcon` (`glib/gio/gicon.c` `g_icon_serialize`): a bare
/// string (`g_icon_new_for_string`) or the tagged `(sv)` union — `file`/
/// `themed`/`bytes`. Emblemed/other forms → no icon.
fn deserialize_gicon(value: &zvariant::OwnedValue) -> Option<NotificationIcon> {
    // Bare-string form.
    if let Ok(clone) = value.try_clone() {
        if let Ok(s) = String::try_from(clone) {
            return NotificationIcon::from_string(&s).and_then(resolve_file_icon);
        }
    }
    // Tagged `(sv)` union.
    let structure = zvariant::Structure::try_from(value.try_clone().ok()?).ok()?;
    let fields = structure.fields();
    if fields.len() != 2 {
        return None;
    }
    let zvariant::Value::Str(tag) = &fields[0] else {
        return None;
    };
    // The second field is a `v`; unwrap one variant layer.
    let inner = match &fields[1] {
        zvariant::Value::Value(v) => v.as_ref(),
        other => other,
    };
    let inner = zvariant::OwnedValue::try_from(inner.try_clone().ok()?).ok()?;
    match tag.as_str() {
        "file" => {
            // v is the file URI (`g_file_get_uri`).
            let uri = String::try_from(inner).ok()?;
            NotificationIcon::from_string(&uri).and_then(resolve_file_icon)
        }
        "themed" => {
            // v is `as`; take the first (highest-priority) name.
            let names = Vec::<String>::try_from(inner).ok()?;
            NotificationIcon::from_string(names.first()?)
        }
        "bytes" => {
            // v is `ay`: raw image-file bytes (`GBytesIcon`).
            let bytes = Vec::<u8>::try_from(inner).ok()?;
            decode_bytes_icon(&bytes)
        }
        _ => None,
    }
}

/// `priority` string (`low`/`normal`/`high`/`urgent`) else the `urgent` bool
/// (`js/ui/notificationDaemon.js:407-437`). gnome-shell's HIGH has no equivalent
/// in our three-level model, so `high` maps to Normal (recorded divergence).
fn parse_urgency(dict: &Dict) -> Urgency {
    if let Some(priority) = dict_str(dict, "priority") {
        return match priority.as_str() {
            "low" => Urgency::Low,
            "urgent" => Urgency::Critical,
            _ => Urgency::Normal,
        };
    }
    if let Some(urgent) = dict.get("urgent").and_then(|v| {
        let v = v.try_clone().ok()?;
        bool::try_from(v).ok()
    }) {
        return if urgent {
            Urgency::Critical
        } else {
            Urgency::Normal
        };
    }
    Urgency::Normal
}

/// The `buttons` array (`aa{sv}`), each `{label, action, target}`; the `target`
/// (`av`) stays server-side (slice 2) so the model carries only plain strings.
fn parse_buttons(dict: &Dict) -> Vec<(String, String)> {
    let Some(value) = dict.get("buttons") else {
        return Vec::new();
    };
    let Ok(clone) = value.try_clone() else {
        return Vec::new();
    };
    let Ok(buttons) = Vec::<Dict>::try_from(clone) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for button in &buttons {
        if out.len() >= 16 {
            break;
        }
        let (Some(action), Some(label)) = (dict_str(button, "action"), dict_str(button, "label"))
        else {
            continue;
        };
        out.push((
            clamp_text(action, 1024),
            clamp_text(flatten_text(&label), 1024),
        ));
    }
    out
}

#[interface(name = "org.gtk.Notifications")]
impl GtkNotifications {
    async fn add_notification(
        &self,
        app_id: String,
        id: String,
        notification: Dict,
    ) -> Result<(), GtkError> {
        let _guard = self.serial.lock().await;

        // Reject unknown apps up front, like gnome-shell's `_ensureAppSource`
        // (`js/ui/notificationDaemon.js:597-616,676-684`) — this decides
        // `InvalidApp` on the untrusted side, before the model is touched.
        let Some(app) = resolve_app(&app_id) else {
            return Err(GtkError::InvalidApp(format!(
                "The app by ID \"{app_id}\" could not be found"
            )));
        };

        let req = GtkNotifyRequest {
            app_title: clamp_text(app.name.unwrap_or_else(|| app_id.clone()), 1024),
            app_icon: app.icon,
            // The summary displays verbatim; only the body is markup-capable
            // (mirrors the fdo path).
            title: clamp_text(
                flatten_text(&dict_str(&notification, "title").unwrap_or_default()),
                4096,
            ),
            body: clamp_text(
                sanitize_text(&dict_str(&notification, "body").unwrap_or_default()),
                8192,
            ),
            icon: notification.get("icon").and_then(deserialize_gicon),
            actions: parse_buttons(&notification),
            default_action: dict_str(&notification, "default-action").map(|s| clamp_text(s, 1024)),
            urgency: parse_urgency(&notification),
            app_id: clamp_text(app_id, 255),
            gtk_id: clamp_text(id, 255),
        };

        self.to_niri
            .send(NotificationsToNiri::AddGtk { req })
            .map_err(|err| {
                warn!("gtk-notifications: error sending message to niri: {err:?}");
                GtkError::ZBus(zbus::Error::Failure("internal error".to_owned()))
            })?;
        Ok(())
    }

    async fn remove_notification(&self, app_id: String, id: String) {
        let _guard = self.serial.lock().await;
        if self
            .to_niri
            .send(NotificationsToNiri::RemoveGtk { app_id, gtk_id: id })
            .is_err()
        {
            warn!("gtk-notifications: error sending RemoveNotification to niri");
        }
    }

    /// Emitted (broadcast) for non-`app.` actions; `app.` actions route to the
    /// app instead (slice 2). Declared for introspection; emission is raw in
    /// `start` (see the emitter task).
    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        app_id: &str,
        id: &str,
        action: &str,
        parameter: Vec<zvariant::Value<'_>>,
        platform_data: HashMap<String, zvariant::Value<'_>>,
    ) -> zbus::Result<()>;
}

impl Start for GtkNotifications {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let from_niri = self.from_niri.clone();
        let conn = zbus::blocking::Connection::session()?;

        conn.object_server().at(PATH, self)?;

        // Own the name with REPLACE only (no AllowReplacement), like the fdo
        // server and gnome-shell's daemon: a later daemon must not steal it, and
        // an already-owned name (`Ok(Exists)`) is a failed start, not an error.
        let flags = RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue;
        match conn.request_name_with_flags(BUS_NAME, flags)? {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => (),
            reply => anyhow::bail!(
                "{BUS_NAME} is owned by another notification daemon \
                 (request replied {reply:?}); Gtk notifications server disabled"
            ),
        }

        // Emitter task: broadcast `ActionInvoked` for non-`app.` actions. The
        // main loop never touches this connection (the process-isolation seam).
        let emit_conn = conn.inner().clone();
        conn.inner()
            .executor()
            .spawn(
                async move {
                    while let Ok(msg) = from_niri.recv().await {
                        match msg {
                            GtkToNotifications::ActionInvoked {
                                app_id,
                                gtk_id,
                                action,
                                token,
                            } => {
                                if action.starts_with("app.") {
                                    // Routed to org.freedesktop.Application in slice 2.
                                    debug!(
                                        "gtk-notifications: app-action {action:?} routing \
                                         deferred to slice 2"
                                    );
                                    continue;
                                }
                                // `emitActionInvoked` includes the activation
                                // token in `platform_data`
                                // (`js/ui/notificationDaemon.js:508-534`); the
                                // target `av` is empty until slice 2.
                                let mut platform_data = HashMap::new();
                                platform_data.insert(
                                    "activation-token".to_owned(),
                                    zvariant::Value::from(token),
                                );
                                let body = (
                                    app_id,
                                    gtk_id,
                                    action,
                                    Vec::<zvariant::Value>::new(),
                                    platform_data,
                                );
                                if let Err(err) = emit_conn
                                    .emit_signal(
                                        Option::<BusName>::None,
                                        PATH,
                                        BUS_NAME,
                                        "ActionInvoked",
                                        &body,
                                    )
                                    .await
                                {
                                    warn!(
                                        "gtk-notifications: error emitting ActionInvoked: {err:?}"
                                    );
                                }
                            }
                        }
                    }
                },
                "gtk notifications signal emitter",
            )
            .detach();

        Ok(conn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_id_validity_rejects_traversal_and_bare_names() {
        assert!(app_id_is_valid("org.gnome.TextEditor"));
        assert!(app_id_is_valid("com.example.App-1_2"));
        // No dot → not a valid application id.
        assert!(!app_id_is_valid("firefox"));
        // Path-traversal / separators must never reach the lookup.
        assert!(!app_id_is_valid("../../etc/passwd"));
        assert!(!app_id_is_valid("a/b.desktop"));
        assert!(!app_id_is_valid("a..b"));
        assert!(!app_id_is_valid(".hidden"));
        assert!(!app_id_is_valid("trailing."));
        assert!(!app_id_is_valid(""));
        assert!(!app_id_is_valid(&"x.".repeat(200)));
    }

    #[test]
    fn desktop_entry_reads_name_and_icon() {
        let dir = std::env::temp_dir().join(format!("gsrs-gtk-notif-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("org.example.App.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Example App\n\
             Name[de]=Beispiel\n\
             Icon=example-icon\n\
             # a comment\n\
             [Desktop Action New]\n\
             Name=Should Be Ignored\n",
        )
        .unwrap();

        let info = read_desktop_entry(&path).unwrap();
        assert_eq!(info.name.as_deref(), Some("Example App"));
        assert_eq!(
            info.icon,
            Some(NotificationIcon::Themed("example-icon".to_owned()))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn desktop_entry_without_name_returns_none_name() {
        let dir = std::env::temp_dir().join(format!("gsrs-gtk-noname-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.desktop");
        std::fs::write(&path, "[Desktop Entry]\nType=Application\n").unwrap();
        let info = read_desktop_entry(&path).unwrap();
        assert_eq!(info.name, None);
        assert_eq!(info.icon, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Round-trip a representative `AddNotification` `a{sv}` payload through the
    /// D-Bus wire encoding and back, so the `a{sv}` / nested-variant unwrapping
    /// the parsers rely on is exercised exactly as zbus delivers it — not just
    /// against hand-built in-memory values.
    fn wire_dict(map: HashMap<String, zvariant::Value<'_>>) -> Dict {
        let ctxt = zvariant::serialized::Context::new_dbus(zvariant::Endian::Little, 0);
        let encoded = zvariant::to_bytes(ctxt, &map).unwrap();
        let (decoded, _): (Dict, _) = encoded.deserialize().unwrap();
        decoded
    }

    #[test]
    fn parses_a_full_gtk_payload_off_the_wire() {
        use zvariant::Value;

        let mut map: HashMap<String, Value> = HashMap::new();
        map.insert("title".into(), Value::from("Hello"));
        map.insert("body".into(), Value::from("<b>world</b> &amp; more"));
        map.insert("priority".into(), Value::from("urgent"));
        map.insert("default-action".into(), Value::from("app.open"));
        // A themed GIcon: ("themed", <['icon-a','icon-b']>).
        let gicon = Value::from((
            "themed".to_string(),
            Value::from(vec!["icon-a".to_string(), "icon-b".to_string()]),
        ));
        map.insert("icon".into(), gicon);
        // buttons: aa{sv}, each {label, action}.
        let mut button: HashMap<String, Value> = HashMap::new();
        button.insert("label".into(), Value::from("Reply"));
        button.insert("action".into(), Value::from("app.reply"));
        map.insert("buttons".into(), Value::from(vec![button]));

        let dict = wire_dict(map);

        assert_eq!(dict_str(&dict, "title").as_deref(), Some("Hello"));
        assert_eq!(parse_urgency(&dict), Urgency::Critical);
        assert_eq!(
            dict_str(&dict, "default-action").as_deref(),
            Some("app.open")
        );
        // The themed GIcon resolves to its first (highest-priority) name.
        assert_eq!(
            deserialize_gicon(dict.get("icon").unwrap()),
            Some(NotificationIcon::Themed("icon-a".to_owned()))
        );
        assert_eq!(
            parse_buttons(&dict),
            vec![("app.reply".to_owned(), "Reply".to_owned())]
        );
    }

    #[test]
    fn parses_string_and_urgent_bool_forms() {
        use zvariant::Value;

        // A bare-string GIcon (g_icon_new_for_string) → themed name.
        let mut map: HashMap<String, Value> = HashMap::new();
        map.insert("icon".into(), Value::from("single-name"));
        map.insert("urgent".into(), Value::from(true));
        let dict = wire_dict(map);
        assert_eq!(
            deserialize_gicon(dict.get("icon").unwrap()),
            Some(NotificationIcon::Themed("single-name".to_owned()))
        );
        // No `priority`, `urgent=true` → Critical.
        assert_eq!(parse_urgency(&dict), Urgency::Critical);
    }
}
