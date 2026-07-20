//! The notifications model: sources, notifications, and the banner queue.
//!
//! This is the fork-owned, observable model behind all four notification
//! surfaces (banner overlay, calendar message list, panel indicator, and the
//! `org.freedesktop.Notifications` D-Bus server in
//! `src/dbus/freedesktop_notifications.rs`), ported from gnome-shell 50.1's
//! `js/ui/messageTray.js` + `js/ui/notificationDaemon.js` and its fdo proxy
//! (`js/dbusServices/notifications/notificationDaemon.js`).
//!
//! Process-isolation seam: notifications are untrusted application content, so
//! everything crossing into this model is plain, validated data. The D-Bus
//! server does ALL untrusted parsing (hints, pixel buffers, markup) on its side
//! and talks to the compositor exclusively over two message channels
//! ([`NotificationsToNiri`] in, [`NiriToNotifications`] out) so it can be
//! lifted into a separate process later without touching the model. Mutations
//! here are pure and return [`Effects`] describing the signals to emit and the
//! banner-surface change; the main loop applies them.
//!
//! Faithfulness notes (all cited inline): client `expire_timeout` is ignored,
//! banner timing is the tray's own; hiding a banner does NOT destroy the
//! notification unless the `transient` hint is set; LOW urgency never banners;
//! CRITICAL bypasses both DND and the queue cap and never auto-expires.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// `MAX_NOTIFICATIONS_PER_SOURCE` (`js/ui/messageTray.js:25`).
pub const MAX_NOTIFICATIONS_PER_SOURCE: usize = 10;
/// `MAX_NOTIFICATIONS_IN_QUEUE` (`js/ui/messageTray.js:24`) — counts the
/// currently-showing banner too (`js/ui/messageTray.js:945-948`).
pub const MAX_NOTIFICATIONS_IN_QUEUE: usize = 3;

/// Bound on untrusted `image-data` dimensions; anything larger is dropped.
const MAX_PIXEL_ICON_DIM: i32 = 4096;

/// fdo urgency levels (`js/ui/notificationDaemon.js:24-29`), mapped 1:1 onto
/// gnome-shell's internal enum minus `HIGH`, which only the (deferred) Gtk
/// daemon can produce (`js/ui/notificationDaemon.js:243-253`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    /// Parse the `urgency` hint byte; absent/unknown defaults to Normal
    /// (`js/ui/notificationDaemon.js:144`).
    pub fn from_wire(value: u32) -> Self {
        match value {
            0 => Urgency::Low,
            2 => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }
}

/// Why a notification was destroyed — the fdo `NotificationClosed` reason codes
/// (`js/ui/notificationDaemon.js:16-22`). gnome-shell's internal
/// `NotificationDestroyedReason` maps EXPIRED→1, DISMISSED→2, SOURCE_CLOSED→3,
/// REPLACED→4 (`js/ui/notificationDaemon.js:180-194`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    Expired,
    Dismissed,
    /// `CloseNotification` / source teardown (internal SOURCE_CLOSED).
    AppClosed,
    Undefined,
}

impl CloseReason {
    pub fn wire_code(self) -> u32 {
        match self {
            CloseReason::Expired => 1,
            CloseReason::Dismissed => 2,
            CloseReason::AppClosed => 3,
            CloseReason::Undefined => 4,
        }
    }
}

/// A decoded, tightly-packed RGBA8 image from the `image-data` hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PixelIcon {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major, no padding.
    pub rgba: Vec<u8>,
}

impl PixelIcon {
    /// Validate + convert the fdo `image-data` wire tuple `(iiibiiay)` —
    /// width, height, rowstride, has_alpha, bits_per_sample, channels, data
    /// (`js/ui/notificationDaemon.js:44-55`). Real clients (Firefox, Telegram)
    /// send rowstride-padded and/or alphaless buffers, so rows are re-packed
    /// and RGB expanded to RGBA here, on the untrusted side of the seam.
    pub fn from_wire(
        width: i32,
        height: i32,
        rowstride: i32,
        has_alpha: bool,
        bits_per_sample: i32,
        channels: i32,
        data: &[u8],
    ) -> Option<Self> {
        if bits_per_sample != 8 {
            return None;
        }
        let expected_channels = if has_alpha { 4 } else { 3 };
        if channels != expected_channels {
            return None;
        }
        if width <= 0 || height <= 0 || width > MAX_PIXEL_ICON_DIM || height > MAX_PIXEL_ICON_DIM {
            return None;
        }
        let (width, height) = (width as usize, height as usize);
        let channels = channels as usize;
        let row_bytes = width * channels;
        let rowstride = usize::try_from(rowstride).ok()?;
        if rowstride < row_bytes {
            return None;
        }
        // The last row need not be padded out to rowstride.
        let needed = rowstride.checked_mul(height - 1)?.checked_add(row_bytes)?;
        if data.len() < needed {
            return None;
        }

        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let row = &data[row * rowstride..row * rowstride + row_bytes];
            if has_alpha {
                rgba.extend_from_slice(row);
            } else {
                for px in row.chunks_exact(3) {
                    rgba.extend_from_slice(px);
                    rgba.push(255);
                }
            }
        }
        Some(Self {
            width: width as u32,
            height: height as u32,
            rgba,
        })
    }
}

/// A notification or source icon, already resolved from the untrusted inputs.
///
/// The notification's own icon comes only from the `image-data`/`image-path`
/// hints; the `app_icon` call parameter is only ever a fallback for the
/// *source* icon (`js/ui/notificationDaemon.js:265-266`).
#[derive(Debug, Clone, PartialEq)]
pub enum NotificationIcon {
    Themed(String),
    File(PathBuf),
    /// `Arc` so store snapshots never deep-copy pixel payloads.
    Pixels(Arc<PixelIcon>),
}

impl NotificationIcon {
    /// gnome-shell's `_iconForNotificationData` (`js/ui/notificationDaemon.js:62-72`):
    /// `file://` URI or absolute path → file icon, anything else → themed name.
    pub fn from_string(s: &str) -> Option<Self> {
        if s.is_empty() {
            return None;
        }
        if let Some(path) = s.strip_prefix("file://") {
            return Some(NotificationIcon::File(PathBuf::from(path)));
        }
        if s.starts_with('/') {
            return Some(NotificationIcon::File(PathBuf::from(s)));
        }
        Some(NotificationIcon::Themed(s.to_owned()))
    }
}

/// How notifications group into a source, mirroring gnome-shell's keying
/// (`js/ui/notificationDaemon.js:74-133`) minus `Shell.App`/WindowTracker: the
/// `desktop-entry` hint when present, else (pid, app_name) with the pid the
/// D-Bus server resolved from the sender (GNOME's fdo proxy injects it the
/// same way, `js/dbusServices/notifications/notificationDaemon.js:99-121`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceKey {
    DesktopEntry(String),
    PidName(u32, String),
}

/// A fully-parsed, validated `Notify()` call, produced by the D-Bus server
/// (the untrusted side of the seam) and consumed by [`NotificationStore::notify`].
#[derive(Debug, Clone, PartialEq)]
pub struct NotifyRequest {
    /// The caller's unique bus name; signals about this notification are
    /// emitted unicast back to it.
    pub sender: Option<String>,
    /// Resolved via `GetConnectionUnixProcessID`; 0 = unknown.
    pub pid: u32,
    pub app_name: String,
    pub replaces_id: u32,
    pub desktop_entry: Option<String>,
    /// From the `app_icon` call parameter (source-icon fallback only).
    pub source_icon: Option<NotificationIcon>,
    /// Sanitized (see [`sanitize_text`]).
    pub title: String,
    /// Sanitized (see [`sanitize_text`]).
    pub body: String,
    pub icon: Option<NotificationIcon>,
    /// (action key, label) pairs, excluding the special `default` action.
    pub actions: Vec<(String, String)>,
    pub has_default_action: bool,
    pub urgency: Urgency,
    pub resident: bool,
    pub transient: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub id: u32,
    pub sender: Option<String>,
    pub title: String,
    pub body: String,
    pub icon: Option<NotificationIcon>,
    pub actions: Vec<(String, String)>,
    pub has_default_action: bool,
    pub urgency: Urgency,
    pub resident: bool,
    pub transient: bool,
    /// Set when its banner is shown (`js/ui/messageTray.js:1167`) or when the
    /// calendar message list opens (`js/ui/messageList.js:1193-1199`); reset by
    /// a replace (`js/ui/notificationDaemon.js:211`).
    pub acknowledged: bool,
    /// Clock-based (pinned in tests). Refreshed by any change except
    /// acknowledgement (`js/ui/messageTray.js:364-380`).
    pub timestamp: Duration,
}

/// One app's notifications, FIFO like gnome-shell's `Source.notifications`
/// (`js/ui/messageTray.js:528`). A source with zero notifications removes
/// itself (`js/ui/messageTray.js:566-577`).
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub key: SourceKey,
    /// The unique bus name that created the source, for sender-vanish teardown.
    pub sender: Option<String>,
    /// The app name; the UI falls back to "Unknown App" when empty
    /// (`js/ui/messageList.js:396-403`).
    pub title: String,
    pub icon: Option<NotificationIcon>,
    pub notifications: Vec<Notification>,
}

impl Source {
    pub fn unseen_count(&self) -> usize {
        self.notifications
            .iter()
            .filter(|n| !n.acknowledged)
            .count()
    }
}

/// A `NotificationClosed` emission owed to the server after a mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedSignal {
    pub id: u32,
    pub reason: CloseReason,
    pub sender: Option<String>,
}

/// What the banner surface must do after a mutation (`M1` contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerEffect {
    /// A banner became available; show it when idle ([`NotificationStore::pop_next_banner`]).
    QueueChanged,
    /// The currently-shown banner's notification changed in place — redraw it
    /// and re-arm its timeout (`js/ui/messageTray.js:938-943`).
    RefreshCurrent,
    /// The currently-shown banner's notification is gone from the model — hide
    /// without animation and skip the transient-destroy at hide-complete
    /// (`js/ui/messageTray.js:909-917,1282`).
    HideCurrent,
}

/// The outcome of a store mutation, applied by the main loop: signals to emit
/// (via [`NiriToNotifications`]) and the banner-surface change.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Effects {
    pub closed: Vec<ClosedSignal>,
    pub banner: Option<BannerEffect>,
}

impl Effects {
    pub fn is_empty(&self) -> bool {
        self.closed.is_empty() && self.banner.is_none()
    }
}

/// `Notify`/`CloseNotification` referenced a live id owned by a different
/// sender → `InvalidArgs`, per gnome-shell's fdo proxy
/// (`js/dbusServices/notifications/notificationDaemon.js:76-90`). Unknown/dead
/// ids are NOT an error (new id on notify, no-op on close).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidId;

/// Messages from the D-Bus server into the compositor.
pub enum NotificationsToNiri {
    Notify {
        req: NotifyRequest,
        reply: async_channel::Sender<Result<u32, InvalidId>>,
    },
    Close {
        id: u32,
        sender: String,
        reply: async_channel::Sender<Result<(), InvalidId>>,
    },
    /// A unique bus name left the bus.
    SenderVanished(String),
}

/// Emit commands from the compositor back to the D-Bus server, which owns the
/// connection and performs the actual (unicast) signal emission.
pub enum NiriToNotifications {
    Closed {
        id: u32,
        reason: CloseReason,
        sender: Option<String>,
    },
}

/// The authoritative store behind all notification surfaces.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationStore {
    /// Newest-active-first: a new notification moves its source to the front
    /// (gnome-shell re-sorts the group on `notification-added`,
    /// `js/ui/messageList.js:1815-1827`).
    pub sources: Vec<Source>,
    /// Ids waiting for a banner, urgency-sorted (stable), excluding the one
    /// currently showing (`js/ui/messageTray.js:951-953`).
    pub banner_queue: Vec<u32>,
    pub current_banner: Option<u32>,
    /// Ids start at 1 (`js/ui/notificationDaemon.js:40,171`).
    next_id: u32,
}

impl Default for NotificationStore {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
            banner_queue: Vec::new(),
            current_banner: None,
            next_id: 1,
        }
    }
}

impl NotificationStore {
    pub fn find(&self, id: u32) -> Option<&Notification> {
        self.sources
            .iter()
            .flat_map(|s| s.notifications.iter())
            .find(|n| n.id == id)
    }

    fn find_mut(&mut self, id: u32) -> Option<&mut Notification> {
        self.sources
            .iter_mut()
            .flat_map(|s| s.notifications.iter_mut())
            .find(|n| n.id == id)
    }

    pub fn unseen_count(&self) -> usize {
        self.sources.iter().map(Source::unseen_count).sum()
    }

    /// The panel indicator count: unseen minus still-queued-for-banner
    /// (`js/ui/dateMenu.js:787-793`).
    pub fn indicator_count(&self) -> usize {
        self.unseen_count().saturating_sub(self.banner_queue.len())
    }

    /// Handle a `Notify()` call: replace in place when `replaces_id` names a
    /// live notification of the same sender (`js/ui/notificationDaemon.js:160-213`),
    /// else allocate the next id. Returns the id to reply plus the effects.
    pub fn notify(
        &mut self,
        req: NotifyRequest,
        show_banners: bool,
        now: Duration,
    ) -> Result<(u32, Effects), InvalidId> {
        let mut effects = Effects::default();

        // A `sender: None` notification is untracked (its creator left the
        // bus): anyone may replace it and adopt it, exactly like gnome-shell's
        // proxy untracking dead senders' ids so a second `notify-send -r`
        // (always a fresh connection) passes the check and the shell daemon
        // reuses the id (`js/dbusServices/notifications/notificationDaemon.js:67-90`).
        let replace_id = (req.replaces_id != 0)
            .then(|| self.find(req.replaces_id))
            .flatten()
            .map(|existing| {
                if existing.sender.is_none() || existing.sender == req.sender {
                    Ok(existing.id)
                } else {
                    Err(InvalidId)
                }
            })
            .transpose()?;

        let id = if let Some(id) = replace_id {
            // Replace: mutate the same notification; every field is re-applied
            // and `acknowledged` reset; no NotificationClosed is emitted
            // (`js/ui/notificationDaemon.js:205-213`). Any change except
            // acknowledgement refreshes the timestamp (`js/ui/messageTray.js:364-380`).
            let notification = self.find_mut(id).unwrap();
            notification.sender = req.sender.clone();
            notification.title = req.title;
            notification.body = req.body;
            notification.icon = req.icon;
            notification.actions = req.actions;
            notification.has_default_action = req.has_default_action;
            notification.urgency = req.urgency;
            notification.resident = req.resident;
            notification.transient = req.transient;
            notification.acknowledged = false;
            notification.timestamp = now;
            // The source's presentation follows every Notify, replace included
            // (`processNotification` runs outside the replace branch,
            // `js/ui/notificationDaemon.js:263-266`).
            let source = self
                .sources
                .iter_mut()
                .find(|s| s.notifications.iter().any(|n| n.id == id))
                .unwrap();
            source.sender = req.sender;
            source.title = req.app_name;
            if req.source_icon.is_some() {
                source.icon = req.source_icon;
            }
            id
        } else {
            let id = self.next_id;
            self.next_id += 1;

            let key = match &req.desktop_entry {
                Some(entry) => SourceKey::DesktopEntry(entry.clone()),
                None => SourceKey::PidName(req.pid, req.app_name.clone()),
            };

            // Evict oldest-first past the per-source cap BEFORE pushing
            // (`js/ui/messageTray.js:579-601`).
            if let Some(source) = self.sources.iter().find(|s| s.key == key) {
                let excess =
                    (source.notifications.len() + 1).saturating_sub(MAX_NOTIFICATIONS_PER_SOURCE);
                let evict: Vec<u32> = source.notifications[..excess]
                    .iter()
                    .map(|n| n.id)
                    .collect();
                for evicted in evict {
                    merge(&mut effects, self.close(evicted, CloseReason::Expired));
                }
            }

            let notification = Notification {
                id,
                sender: req.sender.clone(),
                title: req.title,
                body: req.body,
                icon: req.icon,
                actions: req.actions,
                has_default_action: req.has_default_action,
                urgency: req.urgency,
                resident: req.resident,
                transient: req.transient,
                acknowledged: false,
                timestamp: now,
            };

            let idx = match self.sources.iter().position(|s| s.key == key) {
                Some(idx) => idx,
                None => {
                    self.sources.insert(
                        0,
                        Source {
                            key,
                            sender: req.sender.clone(),
                            title: String::new(),
                            icon: None,
                            notifications: Vec::new(),
                        },
                    );
                    0
                }
            };
            let source = self.sources.remove(idx);
            self.sources.insert(0, source);
            let source = &mut self.sources[0];
            // The source's presentation and tracked sender follow the latest
            // Notify (`js/ui/notificationDaemon.js:265-266` re-processes each
            // time; a restarted app re-keys to the same source).
            source.sender = req.sender.clone();
            source.title = req.app_name;
            if req.source_icon.is_some() {
                source.icon = req.source_icon;
            }
            source.notifications.push(notification);
            id
        };

        if let Some(banner) = self.request_banner(id, show_banners) {
            // A close-driven effect (eviction hitting the current banner) is
            // superseded by the new admission only if there is none.
            effects.banner = Some(match effects.banner {
                Some(BannerEffect::HideCurrent) if banner != BannerEffect::RefreshCurrent => {
                    BannerEffect::HideCurrent
                }
                _ => banner,
            });
        }
        Ok((id, effects))
    }

    /// The single banner-admission gate, shared by notify and replace
    /// (`js/ui/messageTray.js:927-958`): acknowledged never banners, LOW never
    /// banners, DND suppresses all but CRITICAL, the queue caps at
    /// [`MAX_NOTIFICATIONS_IN_QUEUE`] counting the showing banner (CRITICAL
    /// bypasses the cap), and the queue stays urgency-sorted (stable).
    fn request_banner(&mut self, id: u32, show_banners: bool) -> Option<BannerEffect> {
        let notification = self.find(id)?;
        if notification.acknowledged {
            return None;
        }
        if notification.urgency == Urgency::Low {
            return None;
        }
        if !show_banners && notification.urgency != Urgency::Critical {
            return None;
        }
        if self.current_banner == Some(id) {
            // Refreshing the shown banner re-acknowledges it, exactly like the
            // first show: `_updateShowingNotification` runs for both and its
            // first act is acking (`js/ui/messageTray.js:938-943,1166-1168`).
            self.find_mut(id).unwrap().acknowledged = true;
            return Some(BannerEffect::RefreshCurrent);
        }
        if self.banner_queue.contains(&id) {
            self.sort_queue();
            return Some(BannerEffect::QueueChanged);
        }
        let count = self.banner_queue.len() + usize::from(self.current_banner.is_some());
        if count >= MAX_NOTIFICATIONS_IN_QUEUE && notification.urgency != Urgency::Critical {
            return None;
        }
        self.banner_queue.push(id);
        self.sort_queue();
        Some(BannerEffect::QueueChanged)
    }

    fn sort_queue(&mut self) {
        let urgency = |id: &u32| {
            self.sources
                .iter()
                .flat_map(|s| s.notifications.iter())
                .find(|n| n.id == *id)
                .map(|n| n.urgency)
                .unwrap_or_default()
        };
        // Stable, descending by urgency (`js/ui/messageTray.js:951-953`).
        let mut queue = std::mem::take(&mut self.banner_queue);
        queue.sort_by_key(|id| std::cmp::Reverse(urgency(id)));
        self.banner_queue = queue;
    }

    /// Destroy a notification. Removes it from its source (removing the source
    /// itself when it empties), drops it from the banner queue, and reports
    /// the `NotificationClosed` emission. Unknown ids are a no-op.
    pub fn close(&mut self, id: u32, reason: CloseReason) -> Effects {
        let mut effects = Effects::default();
        let Some(src_idx) = self
            .sources
            .iter()
            .position(|s| s.notifications.iter().any(|n| n.id == id))
        else {
            return effects;
        };
        let source = &mut self.sources[src_idx];
        let idx = source
            .notifications
            .iter()
            .position(|n| n.id == id)
            .unwrap();
        let notification = source.notifications.remove(idx);
        if source.notifications.is_empty() {
            self.sources.remove(src_idx);
        }

        effects.closed.push(ClosedSignal {
            id,
            reason,
            sender: notification.sender,
        });
        if self.current_banner == Some(id) {
            self.current_banner = None;
            effects.banner = Some(BannerEffect::HideCurrent);
        } else if let Some(pos) = self.banner_queue.iter().position(|&q| q == id) {
            self.banner_queue.remove(pos);
            effects.banner = Some(BannerEffect::QueueChanged);
        }
        effects
    }

    /// `CloseNotification` from the bus: a live id owned by a different,
    /// still-tracked sender is `InvalidArgs`; unknown ids succeed as a no-op,
    /// and untracked (`sender: None`) notifications may be closed by anyone
    /// (the fdo proxy's `_checkNotificationId`,
    /// `js/dbusServices/notifications/notificationDaemon.js:76-90`).
    pub fn close_checked(&mut self, id: u32, sender: &str) -> Result<Effects, InvalidId> {
        match self.find(id) {
            None => Ok(Effects::default()),
            Some(n) if n.sender.is_none() || n.sender.as_deref() == Some(sender) => {
                Ok(self.close(id, CloseReason::AppClosed))
            }
            Some(_) => Err(InvalidId),
        }
    }

    /// A unique bus name left: tear down its DesktopEntry-keyed sources
    /// (gnome-shell keeps pid-keyed sources alive on sender vanish so
    /// `notify-send`, which exits immediately, survives —
    /// `js/ui/notificationDaemon.js:340-348`), and untrack the sender on
    /// everything that remains (the proxy's `_untrackSender`,
    /// `js/dbusServices/notifications/notificationDaemon.js:67-74`) so a later
    /// connection may replace/close those ids.
    pub fn sender_vanished(&mut self, sender: &str) -> Effects {
        let mut effects = Effects::default();
        let ids: Vec<u32> = self
            .sources
            .iter()
            .filter(|s| {
                matches!(s.key, SourceKey::DesktopEntry(_)) && s.sender.as_deref() == Some(sender)
            })
            .flat_map(|s| s.notifications.iter().map(|n| n.id))
            .collect();
        for id in ids {
            merge(&mut effects, self.close(id, CloseReason::AppClosed));
        }
        for source in &mut self.sources {
            if source.sender.as_deref() == Some(sender) {
                source.sender = None;
            }
            for notification in &mut source.notifications {
                if notification.sender.as_deref() == Some(sender) {
                    notification.sender = None;
                }
            }
        }
        effects
    }

    /// The calendar message list opened: everything currently in the store is
    /// acknowledged (`js/ui/messageList.js:1193-1199`) and acked entries drop
    /// out of the banner queue without ever showing
    /// (`js/ui/messageTray.js:1070-1078`). Timestamps are NOT refreshed
    /// (acknowledgement is the one change that doesn't, `js/ui/messageTray.js:366-380`).
    pub fn acknowledge_all(&mut self) -> Effects {
        for source in &mut self.sources {
            for notification in &mut source.notifications {
                notification.acknowledged = true;
            }
        }
        let mut effects = Effects::default();
        if !self.banner_queue.is_empty() {
            self.banner_queue.clear();
            effects.banner = Some(BannerEffect::QueueChanged);
        }
        effects
    }

    /// Take the next queued banner to show: purges acked entries, promotes the
    /// head (the queue is urgency-sorted), and marks it acknowledged the way
    /// showing a banner does (`js/ui/messageTray.js:1070-1078,1167`). Returns
    /// `None` while a banner is already showing.
    pub fn pop_next_banner(&mut self) -> Option<u32> {
        if self.current_banner.is_some() {
            return None;
        }
        loop {
            if self.banner_queue.is_empty() {
                return None;
            }
            let id = self.banner_queue.remove(0);
            let Some(notification) = self.find_mut(id) else {
                continue;
            };
            if notification.acknowledged {
                continue;
            }
            notification.acknowledged = true;
            self.current_banner = Some(id);
            return Some(id);
        }
    }

    /// The shown banner finished hiding on its own (timeout/hover-out): the
    /// notification survives unless it's transient, which is destroyed with
    /// reason EXPIRED (`js/ui/messageTray.js:1279-1292`). Not called when the
    /// model itself removed the notification ([`BannerEffect::HideCurrent`]).
    pub fn banner_hidden(&mut self) -> Effects {
        let Some(id) = self.current_banner.take() else {
            return Effects::default();
        };
        match self.find(id) {
            Some(n) if n.transient => self.close(id, CloseReason::Expired),
            _ => Effects::default(),
        }
    }
}

fn merge(into: &mut Effects, other: Effects) {
    into.closed.extend(other.closed);
    if let Some(banner) = other.banner {
        // HideCurrent must not be masked by a later QueueChanged.
        if into.banner != Some(BannerEffect::HideCurrent) {
            into.banner = Some(banner);
        }
    }
}

/// Newline flattening for untrusted TITLE text (`js/ui/messageList.js:564-568`).
/// gnome-shell escapes the whole summary (`Util.fixMarkup(text, false)`), so a
/// title displays verbatim — no tag stripping, no entity unescaping.
pub fn flatten_text(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// The render floor for untrusted BODY text: flatten newlines to spaces
/// (`js/ui/messageList.js:575-578`), strip the b/i/u tags gnome-shell would
/// render (`js/misc/util.js:184-202` allows exactly those), and unescape the
/// predefined XML entities so escaped text reads correctly. Rich rendering of
/// the stripped tags is deferred.
pub fn sanitize_text(text: &str) -> String {
    let mut s = flatten_text(text);
    for tag in ["<b>", "</b>", "<i>", "</i>", "<u>", "</u>"] {
        s = s.replace(tag, "");
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(app: &str, sender: &str) -> NotifyRequest {
        NotifyRequest {
            sender: Some(sender.to_owned()),
            pid: 100,
            app_name: app.to_owned(),
            replaces_id: 0,
            desktop_entry: None,
            source_icon: None,
            title: "title".to_owned(),
            body: "body".to_owned(),
            icon: None,
            actions: Vec::new(),
            has_default_action: false,
            urgency: Urgency::Normal,
            resident: false,
            transient: false,
        }
    }

    fn notify(store: &mut NotificationStore, r: NotifyRequest) -> (u32, Effects) {
        store.notify(r, true, Duration::from_secs(1)).unwrap()
    }

    #[test]
    fn ids_start_at_one_and_are_monotonic() {
        let mut store = NotificationStore::default();
        let (a, _) = notify(&mut store, req("app", ":1.1"));
        let (b, _) = notify(&mut store, req("app", ":1.1"));
        let (c, _) = notify(&mut store, req("other", ":1.2"));
        assert_eq!((a, b, c), (1, 2, 3));
    }

    #[test]
    fn replace_mutates_in_place_without_closed_signal() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("app", ":1.1"));
        // Dismiss the banner admission state: ack it like a shown banner.
        store.pop_next_banner();
        store.banner_hidden();

        let mut update = req("app", ":1.1");
        update.replaces_id = id;
        update.title = "new".to_owned();
        update.urgency = Urgency::Critical;
        let (rid, effects) = store.notify(update, true, Duration::from_secs(5)).unwrap();
        assert_eq!(rid, id);
        assert!(effects.closed.is_empty());
        // The replace re-enters banner admission (H3): the previously-acked
        // notification banners again.
        assert_eq!(effects.banner, Some(BannerEffect::QueueChanged));
        let n = store.find(id).unwrap();
        assert_eq!(n.title, "new");
        assert!(!n.acknowledged);
        // Replace refreshes the timestamp (M2)...
        assert_eq!(n.timestamp, Duration::from_secs(5));
        // ...and there is still exactly one notification.
        assert_eq!(store.sources.len(), 1);
        assert_eq!(store.sources[0].notifications.len(), 1);
    }

    #[test]
    fn ack_does_not_refresh_timestamp() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("app", ":1.1"));
        store.acknowledge_all();
        assert_eq!(store.find(id).unwrap().timestamp, Duration::from_secs(1));
        assert!(store.find(id).unwrap().acknowledged);
    }

    #[test]
    fn replace_of_foreign_sender_is_invalid() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("app", ":1.1"));
        let mut foreign = req("evil", ":1.66");
        foreign.replaces_id = id;
        assert_eq!(
            store.notify(foreign, true, Duration::from_secs(2)),
            Err(InvalidId)
        );
    }

    #[test]
    fn replace_of_dead_id_allocates_new() {
        let mut store = NotificationStore::default();
        let mut r = req("app", ":1.1");
        r.replaces_id = 42;
        let (id, _) = notify(&mut store, r);
        assert_eq!(id, 1);
    }

    #[test]
    fn replace_while_showing_refreshes_and_reacks() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("app", ":1.1"));
        assert_eq!(store.pop_next_banner(), Some(id));

        let mut update = req("app", ":1.1");
        update.replaces_id = id;
        let (_, effects) = store.notify(update, true, Duration::from_secs(2)).unwrap();
        assert_eq!(effects.banner, Some(BannerEffect::RefreshCurrent));
        assert_eq!(store.current_banner, Some(id));
        // Refreshing the shown banner re-acks, exactly like the first show —
        // otherwise the notification would count as unseen forever.
        assert!(store.find(id).unwrap().acknowledged);
        assert_eq!(store.unseen_count(), 0);
    }

    #[test]
    fn replace_updates_source_presentation() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("Old Name", ":1.1"));
        let mut update = req("New Name", ":1.1");
        update.replaces_id = id;
        update.source_icon = Some(NotificationIcon::Themed("new-icon".to_owned()));
        store.notify(update, true, Duration::from_secs(2)).unwrap();
        assert_eq!(store.sources[0].title, "New Name");
        assert_eq!(
            store.sources[0].icon,
            Some(NotificationIcon::Themed("new-icon".to_owned()))
        );
    }

    #[test]
    fn replace_after_sender_vanish_is_adopted() {
        // The real `notify-send -p` → `notify-send -r <id>` flow: every
        // invocation is a fresh bus connection, and the first sender is long
        // gone by the second call. GNOME's proxy untracks the dead sender's
        // ids so the replace passes; we must too.
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("notify-send", ":1.1"));
        store.sender_vanished(":1.1");
        assert!(store.find(id).unwrap().sender.is_none());

        let mut replace = req("notify-send", ":1.2");
        replace.replaces_id = id;
        replace.title = "second".to_owned();
        let (rid, _) = store.notify(replace, true, Duration::from_secs(2)).unwrap();
        assert_eq!(rid, id);
        let n = store.find(id).unwrap();
        assert_eq!(n.title, "second");
        // Adopted: signals now go to the new sender, which may also close it.
        assert_eq!(n.sender.as_deref(), Some(":1.2"));
        assert!(store.close_checked(id, ":1.2").is_ok());
    }

    #[test]
    fn close_checked_rejects_foreign_and_ignores_unknown() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("app", ":1.1"));
        assert_eq!(store.close_checked(id, ":1.66"), Err(InvalidId));
        assert_eq!(store.close_checked(999, ":1.66"), Ok(Effects::default()));
        let effects = store.close_checked(id, ":1.1").unwrap();
        assert_eq!(
            effects.closed,
            vec![ClosedSignal {
                id,
                reason: CloseReason::AppClosed,
                sender: Some(":1.1".to_owned()),
            }]
        );
        assert!(store.sources.is_empty());
    }

    #[test]
    fn eviction_past_source_cap_expires_oldest() {
        let mut store = NotificationStore::default();
        for _ in 0..MAX_NOTIFICATIONS_PER_SOURCE {
            notify(&mut store, req("app", ":1.1"));
        }
        let (_, effects) = notify(&mut store, req("app", ":1.1"));
        assert_eq!(effects.closed.len(), 1);
        assert_eq!(effects.closed[0].id, 1);
        assert_eq!(effects.closed[0].reason, CloseReason::Expired);
        assert_eq!(
            store.sources[0].notifications.len(),
            MAX_NOTIFICATIONS_PER_SOURCE
        );
    }

    #[test]
    fn source_removes_itself_when_empty_and_sources_are_newest_first() {
        let mut store = NotificationStore::default();
        let (a, _) = notify(&mut store, req("first", ":1.1"));
        notify(&mut store, req("second", ":1.2"));
        assert_eq!(store.sources[0].title, "second");
        // A new notification moves its source back to the front.
        notify(&mut store, req("first", ":1.1"));
        assert_eq!(store.sources[0].title, "first");
        store.close(a, CloseReason::Dismissed);
        assert_eq!(store.sources.len(), 2);
        let remaining_first = &store.sources[0];
        assert_eq!(remaining_first.notifications.len(), 1);
    }

    #[test]
    fn queue_caps_at_three_criticals_bypass_and_low_never_banners() {
        let mut store = NotificationStore::default();
        for _ in 0..3 {
            notify(&mut store, req("app", ":1.1"));
        }
        assert_eq!(store.banner_queue.len(), 3);
        // Fourth normal-urgency: over the cap, silently not queued.
        let (_, effects) = notify(&mut store, req("app", ":1.1"));
        assert_eq!(effects.banner, None);
        assert_eq!(store.banner_queue.len(), 3);
        // LOW never banners at all.
        let mut low = req("app", ":1.1");
        low.urgency = Urgency::Low;
        let (_, effects) = notify(&mut store, low);
        assert_eq!(effects.banner, None);
        // CRITICAL bypasses the cap and sorts to the front.
        let mut critical = req("app", ":1.1");
        critical.urgency = Urgency::Critical;
        let (crit_id, effects) = notify(&mut store, critical);
        assert_eq!(effects.banner, Some(BannerEffect::QueueChanged));
        assert_eq!(store.banner_queue.first(), Some(&crit_id));
        assert_eq!(store.banner_queue.len(), 4);
    }

    #[test]
    fn dnd_suppresses_banners_except_critical() {
        let mut store = NotificationStore::default();
        let (_, effects) = store
            .notify(req("app", ":1.1"), false, Duration::from_secs(1))
            .unwrap();
        assert_eq!(effects.banner, None);
        assert!(store.banner_queue.is_empty());
        let mut critical = req("app", ":1.1");
        critical.urgency = Urgency::Critical;
        let (_, effects) = store
            .notify(critical, false, Duration::from_secs(1))
            .unwrap();
        assert_eq!(effects.banner, Some(BannerEffect::QueueChanged));
    }

    #[test]
    fn pop_next_banner_acks_and_skips_acked() {
        let mut store = NotificationStore::default();
        let (a, _) = notify(&mut store, req("app", ":1.1"));
        let (b, _) = notify(&mut store, req("app", ":1.1"));
        store.find_mut(a).unwrap().acknowledged = true;
        assert_eq!(store.pop_next_banner(), Some(b));
        assert!(store.find(b).unwrap().acknowledged);
        assert_eq!(store.current_banner, Some(b));
        // One showing → nothing else pops.
        assert_eq!(store.pop_next_banner(), None);
        // Indicator: everything is either acked or accounted queued.
        assert_eq!(store.indicator_count(), 0);
    }

    #[test]
    fn banner_hidden_destroys_only_transient() {
        let mut store = NotificationStore::default();
        let mut transient = req("app", ":1.1");
        transient.transient = true;
        let (id, _) = notify(&mut store, transient);
        assert_eq!(store.pop_next_banner(), Some(id));
        let effects = store.banner_hidden();
        assert_eq!(effects.closed.len(), 1);
        assert_eq!(effects.closed[0].reason, CloseReason::Expired);
        assert!(store.sources.is_empty());

        let (id, _) = notify(&mut store, req("app", ":1.1"));
        assert_eq!(store.pop_next_banner(), Some(id));
        let effects = store.banner_hidden();
        assert!(effects.is_empty());
        assert!(store.find(id).is_some());
    }

    #[test]
    fn close_of_current_banner_reports_hide_current() {
        let mut store = NotificationStore::default();
        let (id, _) = notify(&mut store, req("app", ":1.1"));
        assert_eq!(store.pop_next_banner(), Some(id));
        let effects = store.close(id, CloseReason::AppClosed);
        assert_eq!(effects.banner, Some(BannerEffect::HideCurrent));
        assert_eq!(store.current_banner, None);
        // The model already destroyed it: banner_hidden must not double-close.
        assert!(store.banner_hidden().is_empty());
    }

    #[test]
    fn acknowledge_all_purges_queue_and_clears_indicator() {
        let mut store = NotificationStore::default();
        notify(&mut store, req("app", ":1.1"));
        notify(&mut store, req("other", ":1.2"));
        assert_eq!(store.banner_queue.len(), 2);
        let effects = store.acknowledge_all();
        assert_eq!(effects.banner, Some(BannerEffect::QueueChanged));
        assert!(store.banner_queue.is_empty());
        assert_eq!(store.unseen_count(), 0);
        assert_eq!(store.indicator_count(), 0);
        assert_eq!(store.pop_next_banner(), None);
    }

    #[test]
    fn sender_vanish_tears_down_desktop_entry_sources_only() {
        let mut store = NotificationStore::default();
        let mut app = req("app", ":1.1");
        app.desktop_entry = Some("org.example.App".to_owned());
        notify(&mut store, app);
        notify(&mut store, req("notify-send", ":1.1"));
        let effects = store.sender_vanished(":1.1");
        assert_eq!(effects.closed.len(), 1);
        assert_eq!(effects.closed[0].reason, CloseReason::AppClosed);
        assert_eq!(store.sources.len(), 1);
        assert!(matches!(store.sources[0].key, SourceKey::PidName(..)));
    }

    #[test]
    fn pixel_icon_depads_rowstride_and_expands_rgb() {
        // 2x2 RGB with rowstride 8 (2 bytes of padding per row).
        let data: Vec<u8> = vec![
            1, 2, 3, 4, 5, 6, 0, 0, // row 0 + pad
            7, 8, 9, 10, 11, 12, // row 1, unpadded tail
        ];
        let icon = PixelIcon::from_wire(2, 2, 8, false, 8, 3, &data).unwrap();
        assert_eq!(icon.width, 2);
        assert_eq!(icon.height, 2);
        assert_eq!(
            icon.rgba,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        // Truncated data is rejected, not sliced.
        assert_eq!(
            PixelIcon::from_wire(2, 2, 8, false, 8, 3, &data[..11]),
            None
        );
        // Channel/alpha mismatch is rejected.
        assert_eq!(PixelIcon::from_wire(2, 2, 8, true, 8, 3, &data), None);
    }

    #[test]
    fn sanitize_flattens_strips_and_unescapes() {
        assert_eq!(
            sanitize_text("a\nb <b>bold</b> &amp; &lt;kept&gt;"),
            "a b bold & <kept>"
        );
    }
}
