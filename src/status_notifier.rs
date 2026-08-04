// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The StatusNotifierItem registry — the model behind app-indicator ("tray") icons.
//!
//! GNOME Shell has **no** StatusNotifier support of any kind, so unlike the rest of the port there
//! is no `js/ui/` file behind this. The reference is the
//! `gnome-shell-extension-appindicator` extension (v64), and the whole feature is a deliberate
//! divergence — see `docs/fork/status-notifier-port.md` for why we carry it and what the slices
//! are. Citations below name that extension's files.
//!
//! This module is the compositor-side half: which items exist, how they are keyed, and what the
//! observable `RegisteredStatusNotifierItems` list says. The bus half — owning
//! `org.kde.StatusNotifierWatcher`, resolving names, watching owners — is in
//! [`crate::dbus::status_notifier`].
//!
//! **Untrusted-content seam.** Every string here was chosen by a client: the registration
//! argument, and later the item's title, icon names and menu labels. Everything crossing into the
//! model is plain, validated, bounded data, so the bus side can be lifted into its own process
//! later.

/// The object path an item is assumed to live at when a client registers by bus name — the KDE
/// convention (`statusNotifierWatcher.js:41`).
pub const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";

/// A bus name is at most this long (the D-Bus spec's limit). Anything longer is not one, and is
/// refused before it reaches a `GetNameOwner` round trip.
const MAX_BUS_NAME_LEN: usize = 255;

/// What a client's `RegisterStatusNotifierItem` argument turned out to mean.
///
/// The spec says "service", and the ecosystem reads that two ways: Ayatana-patched apps send an
/// **object path** and mean "my own bus name, this path", while KDE apps send a **bus name** and
/// mean the well-known `/StatusNotifierItem` (`statusNotifierWatcher.js:207-235`). A watcher that
/// understands only one form silently drops half the clients, so the dispatch is on the leading
/// `/` and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceRef {
    /// The argument was an object path. The item is on the *sender's* connection, so no name
    /// resolution is needed — and no name resolution is wanted: trusting a path-sending client to
    /// also name its own connection would let it register on someone else's behalf.
    Path {
        unique_name: String,
        object_path: String,
    },
    /// The argument was a bus name, which still has to be resolved to a unique name before the
    /// item can be tracked. May itself already be unique (`:1.42`).
    Name { service: String },
}

/// Why a registration argument was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Neither a plausible object path nor a plausible bus name.
    NotABusNameOrPath,
    /// An object path was sent, but the message had no sender to attribute it to. Nothing can be
    /// registered for a peer we cannot name.
    NoSender,
}

/// Classify a `RegisterStatusNotifierItem` argument.
///
/// `sender` is the message's unique name, which is what the argument means when it is a path.
pub fn parse_service_argument(arg: &str, sender: Option<&str>) -> Result<ServiceRef, ParseError> {
    if arg.starts_with('/') {
        if !is_valid_object_path(arg) {
            return Err(ParseError::NotABusNameOrPath);
        }
        let unique_name = sender.ok_or(ParseError::NoSender)?;
        return Ok(ServiceRef::Path {
            unique_name: unique_name.to_owned(),
            object_path: arg.to_owned(),
        });
    }

    if is_plausible_bus_name(arg) {
        return Ok(ServiceRef::Name {
            service: arg.to_owned(),
        });
    }

    Err(ParseError::NotABusNameOrPath)
}

/// A cheap structural check on an object path, so a malformed one is refused here rather than
/// deeper in, where it would be a bus error against a client that cannot see it.
fn is_valid_object_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    if !path.starts_with('/') || path.ends_with('/') || path.contains("//") {
        return false;
    }
    path[1..]
        .split('/')
        .all(|el| !el.is_empty() && el.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Whether a string could be a bus name — well-known (`org.kde.Foo`) or unique (`:1.42`).
///
/// The extension uses a loose regex here (`dbusUtils.js:22`) and so do we: the goal is to reject
/// obvious nonsense before spending a round trip, not to re-implement the bus's own validation.
/// The bus is the authority and will reject what it dislikes.
fn is_plausible_bus_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_BUS_NAME_LEN {
        return false;
    }

    // A unique name's elements may start with a digit; a well-known name's may not.
    let unique = name.starts_with(':');
    let body = name.strip_prefix(':').unwrap_or(name);

    // Both forms need at least one dot, so a bare word is not mistaken for a name.
    if !body.contains('.') {
        return false;
    }

    body.split('.').all(|el| {
        !el.is_empty()
            && el
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && (unique || !el.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// One registered item, as the model holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredItem {
    /// The item's observable identity, published in `RegisteredStatusNotifierItems` and carried by
    /// the registration signals. See [`item_id`].
    pub id: String,
    /// The connection that owns the item. Identity for lifetime purposes: a well-known name can
    /// move between connections, and an item that moved is a different item.
    pub unique_name: String,
    /// Where the item's interface lives on that connection.
    pub object_path: String,
}

/// The item's public id: the well-known name when the client registered by one, and
/// `<unique-name>@<path>` otherwise (`util.js:33-38`).
///
/// Other hosts and some clients read this list back, so the format is not ours to prettify.
pub fn item_id(service: Option<&str>, unique_name: &str, object_path: &str) -> String {
    match service {
        Some(service) if service != unique_name => service.to_owned(),
        _ => format!("{unique_name}@{object_path}"),
    }
}

/// What happened when an item was offered to the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// A new item. The caller owes the registration signal and a property change.
    Added,
    /// This exact item is already registered. The client is re-registering — several do it on
    /// their own restart — and the answer is to refresh it in place, never to add a second icon
    /// (`statusNotifierWatcher.js:134-146`).
    AlreadyRegistered,
}

/// The set of items that have registered with our watcher.
///
/// Keyed by `(unique_name, object_path)` rather than by [`RegisteredItem::id`]: one connection may
/// legitimately export several items on different paths, and the well-known name a client
/// registered under says nothing about which connection is serving it now.
#[derive(Debug, Default)]
pub struct ItemRegistry {
    items: Vec<RegisteredItem>,
}

impl ItemRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an item, or report that it is already there.
    pub fn insert(&mut self, item: RegisteredItem) -> Registration {
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|i| i.unique_name == item.unique_name && i.object_path == item.object_path)
        {
            // The id can legitimately change on a re-registration: a client that first registered
            // by path and then by well-known name is the same item under a better name.
            existing.id = item.id;
            return Registration::AlreadyRegistered;
        }

        self.items.push(item);
        Registration::Added
    }

    /// Drop every item served by `unique_name`, returning their ids so the caller can emit one
    /// unregistration signal each.
    pub fn remove_owner(&mut self, unique_name: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.items.retain(|item| {
            if item.unique_name == unique_name {
                removed.push(item.id.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Drop one item by connection and path — what the liveness probe uses when a client keeps its
    /// bus name but drops the object (see the Electron trap in the port doc).
    pub fn remove_item(&mut self, unique_name: &str, object_path: &str) -> Option<String> {
        let idx = self
            .items
            .iter()
            .position(|i| i.unique_name == unique_name && i.object_path == object_path)?;
        Some(self.items.remove(idx).id)
    }

    pub fn contains_owner(&self, unique_name: &str) -> bool {
        self.items.iter().any(|i| i.unique_name == unique_name)
    }

    pub fn items(&self) -> &[RegisteredItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The `RegisteredStatusNotifierItems` property, in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.items.iter().map(|i| i.id.clone()).collect()
    }
}

/// An item's `Status` (`appIndicator.js:54-58`). Drives visibility and which icon is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemStatus {
    /// The item exists but wants no room in the panel. The default before the first read, so an
    /// item cannot flash into the panel while its properties are still being fetched
    /// (`appIndicator.js:115-116`).
    #[default]
    Passive,
    Active,
    NeedsAttention,
}

impl ItemStatus {
    pub fn parse(value: &str) -> Self {
        match value {
            "Active" => Self::Active,
            "NeedsAttention" => Self::NeedsAttention,
            _ => Self::Passive,
        }
    }

    /// Whether an item in this state occupies the panel (`indicatorStatusIcon.js:321`).
    pub fn is_visible(self) -> bool {
        self != Self::Passive
    }
}

/// An item's `Category` (`appIndicator.js:47-52`). Carried for ordering and introspection; it does
/// not affect drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemCategory {
    #[default]
    ApplicationStatus,
    Communications,
    SystemServices,
    Hardware,
}

impl ItemCategory {
    pub fn parse(value: &str) -> Self {
        match value {
            "Communications" => Self::Communications,
            "SystemServices" => Self::SystemServices,
            "Hardware" => Self::Hardware,
            _ => Self::ApplicationStatus,
        }
    }
}

/// Cap for one untrusted display string, as [`crate::mpris`] caps track text: enough for any real
/// title, small enough that a hostile client cannot buy a multi-megabyte glyph run.
const MAX_TEXT_BYTES: usize = 1024;

/// An icon name longer than this is not one.
const MAX_ICON_NAME_BYTES: usize = 255;

/// The largest pixmap edge we will take from a client. A panel icon is 16 logical px; anything
/// past this is a client sending us its window contents, and the bytes are its choice, not ours.
pub const MAX_PIXMAP_DIM: u32 = 512;

/// A client-supplied bitmap icon, converted to what the compositor composites: premultiplied
/// RGBA, byte order `R,G,B,A` (`Fourcc::Abgr8888` on a little-endian machine).
///
/// The wire form is `a(iiay)` — width, height, and **ARGB32 in network byte order**, i.e. bytes
/// `A,R,G,B`, *not* premultiplied (`appIndicator.js:45` asks Cogl for `ARGB_8888`, the
/// non-premultiplied variant; the extension's `argbToRgba` only reorders,
/// `pixmapsUtils.js:17-30`, because Cogl premultiplies on upload and we do not).
#[derive(Clone)]
pub struct Pixmap {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA, `width * height * 4` bytes.
    pub rgba: std::sync::Arc<[u8]>,
    /// Content hash — the identity the texture cache keys on, and what makes comparing two
    /// `ItemProps` cheap. Two pixmaps with the same hash and size are the same picture.
    pub hash: u64,
}

/// Elides the pixels. Anything holding an [`ItemIcon`] is debug-printable, and a 48x48 pixmap is
/// 9 KB of decimal bytes in a log line that is meant to say which icon an item is using.
impl std::fmt::Debug for Pixmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pixmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("hash", &format_args!("{:016x}", self.hash))
            .finish()
    }
}

impl PartialEq for Pixmap {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.width == other.width && self.height == other.height
    }
}

impl Eq for Pixmap {}

/// Choose which of a client's pixmaps to draw at `preferred_size` physical px.
///
/// The spec lets a client offer the same icon at several sizes. The extension prefers the
/// *smallest* one at least as large as wanted, and falls back to the largest available when
/// every entry is too small (`pixmapsUtils.js:32-67`) — upscaling a 16px icon to 32 is worse than
/// downscaling a 64px one. Entries past [`MAX_PIXMAP_DIM`], or whose byte count does not match
/// their claimed size, are not candidates at all.
///
/// Returns the index into `entries`, whose items are `(width, height, byte_len)`.
pub fn pick_pixmap(entries: &[(i32, i32, usize)], preferred_size: u32) -> Option<usize> {
    let usable: Vec<(usize, u32, u32)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, &(w, h, len))| {
            let w = u32::try_from(w).ok()?;
            let h = u32::try_from(h).ok()?;
            if w == 0 || h == 0 || w > MAX_PIXMAP_DIM || h > MAX_PIXMAP_DIM {
                return None;
            }
            // A client that lies about its dimensions gets no pixels drawn, rather than a read
            // past the end of what it sent.
            if len != (w as usize) * (h as usize) * 4 {
                return None;
            }
            Some((i, w, h))
        })
        .collect();

    // Big enough, smallest first.
    usable
        .iter()
        .filter(|&&(_, w, h)| w >= preferred_size && h >= preferred_size)
        .min_by_key(|&&(_, w, h)| w as u64 * h as u64)
        // Else the largest we were given.
        .or_else(|| usable.iter().max_by_key(|&&(_, w, h)| w as u64 * h as u64))
        .map(|&(i, _, _)| i)
}

/// Convert one wire pixmap into a [`Pixmap`], or `None` if it is not self-consistent.
///
/// This is the untrusted-content boundary for pixels: the bytes came from a client, so the size is
/// bounded, the length is checked against the dimensions, and the conversion cannot read outside
/// what was sent.
pub fn pixmap_from_argb(width: i32, height: i32, argb: &[u8]) -> Option<Pixmap> {
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    if width == 0 || height == 0 || width > MAX_PIXMAP_DIM || height > MAX_PIXMAP_DIM {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if argb.len() != expected {
        return None;
    }

    let mut rgba = Vec::with_capacity(expected);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for px in argb.chunks_exact(4) {
        let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
        // Premultiply: our compositing path takes premultiplied alpha, and the wire form is not.
        // Skipping this makes every translucent edge glow.
        let mul = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
        rgba.extend_from_slice(&[mul(r), mul(g), mul(b), a]);
        for byte in px {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    Some(Pixmap {
        width,
        height,
        rgba: rgba.into(),
        hash,
    })
}

/// One item's properties, as the model holds them: validated, bounded, and free of anything the
/// bus layer knows about. Every string here was chosen by the client.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemProps {
    /// The client's own `Id` — an application identifier, not a display string. Its presence is
    /// what makes an item showable (see [`ItemProps::is_ready`]).
    pub app_id: String,
    /// `Title`, for the accessible name and tooltip-ish uses. Display text: validated.
    pub title: String,
    pub status: ItemStatus,
    pub category: ItemCategory,
    /// The normal icon, in whichever of the three forms the client offered.
    pub icon: ItemIcon,
    /// The attention icon, shown instead while [`ItemStatus::NeedsAttention`].
    pub attention_icon: ItemIcon,
    /// The `Menu` object path, or `None` when the client has no menu — including the explicit
    /// `/NO_DBUSMENU` sentinel (`appIndicator.js:576-580`).
    pub menu_path: Option<String>,
    /// `ItemIsMenu`: the client says a primary click should open the menu rather than activate.
    pub item_is_menu: bool,
    /// Whether the item's interface actually declares `Activate` (`appIndicator.js:446-457`).
    /// Defaults to true so an item is not written off before it has been introspected.
    pub supports_activation: bool,
    /// Whether it declares the Ayatana spelling of secondary activation.
    pub has_ayatana_secondary_activate: bool,
}

impl ItemProps {
    /// Whether the item may be shown.
    ///
    /// **Divergence.** The extension additionally requires a menu — `isReady` is
    /// `hasNameOwner && this.id && this.menuPath` (`appIndicator.js:476-486`), and `menuPath` is
    /// null for `/NO_DBUSMENU` — so an activate-only item never appears in it at all. That is not
    /// what the spec says and not what Plasma does: a menu is optional, and an item with none is
    /// simply one where a click activates. We gate on `Id` alone, which is the part that is
    /// actually about "has the client finished exporting itself".
    pub fn is_ready(&self) -> bool {
        !self.app_id.is_empty()
    }

    /// The icon to draw right now: the attention icon while asking for attention, falling back to
    /// the normal one when the client set a status but no separate icon for it
    /// (`appIndicator.js:1501`).
    pub fn effective_icon(&self) -> &ItemIcon {
        if self.status == ItemStatus::NeedsAttention && !self.attention_icon.is_none() {
            return &self.attention_icon;
        }
        &self.icon
    }
}

/// How a client offered its icon. The three forms are tried in the spec's order of preference —
/// "names are preferred over pixmaps" (`StatusNotifierItem.xml`) — but a client may offer only one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ItemIcon {
    /// Nothing usable was offered.
    #[default]
    None,
    /// A themed icon name, normalized by [`normalize_icon_name`].
    Themed(String),
    /// A file on disk: either an absolute path the client sent *as* the name, or a name resolved
    /// inside its own `IconThemePath`. Loaded through [`crate::image_source::ImageSource::File`],
    /// which is the same validated path album art takes.
    File(std::path::PathBuf),
    /// Raw pixels the client sent over the bus.
    Pixmap(std::sync::Arc<Pixmap>),
}

impl ItemIcon {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Reduce a client's `IconName` to a themed name, or `None` when it is not one.
///
/// Two out-of-spec habits are common enough that the extension handles both
/// (`appIndicator.js:1247-1265`): sending an absolute **path**, and sending a **file name** with a
/// `.png`/`.svg` extension. A path is not a name — [`icon_from_name`] turns those into
/// [`ItemIcon::File`] instead.
pub fn normalize_icon_name(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_ICON_NAME_BYTES || name.starts_with('/') {
        return None;
    }

    // An icon *name* has no path separators, and a themed lookup for one would be a lookup for a
    // file we were never asked to load.
    if name.contains('/') {
        return None;
    }

    let stem = match name.rsplit_once('.') {
        Some((stem, "png" | "svg" | "xpm")) if !stem.is_empty() => stem,
        _ => name,
    };
    Some(stem.to_owned())
}

/// The image file extensions an out-of-theme icon may have, in preference order: SVG scales, so
/// it beats a raster of unknown size.
const ICON_FILE_EXTENSIONS: &[&str] = &["svg", "png", "xpm"];

/// Turn a client's `IconName` plus its optional `IconThemePath` into an [`ItemIcon`].
///
/// Three cases, all seen in the wild:
///
/// 1. **An absolute path.** Out of spec, but `indicator-sensors` does it
///    (`appIndicator.js:1252-1261`). Taken as a file.
/// 2. **A name with an `IconThemePath`.** The client ships its own icons outside any installed
///    theme. The extension builds a private `StIconTheme` whose search path is that directory
///    (`appIndicator.js:1289-1320`); we look inside it directly, because our themed resolver
///    (`resolve_symbolic`) is a fixed walk over installed themes and cannot take a per-item root.
/// 3. **A plain name.** The themed lookup, as before.
///
/// `exists` decides whether a candidate file is really there — injected so the search order can be
/// tested without touching the filesystem.
pub fn icon_from_name(
    name: &str,
    theme_path: Option<&str>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> ItemIcon {
    use std::path::{Path, PathBuf};

    let name = name.trim();
    if name.is_empty() || name.len() > MAX_ICON_NAME_BYTES {
        return ItemIcon::None;
    }

    // (1) An absolute path is a file, not a name.
    if name.starts_with('/') {
        let path = Path::new(name);
        return if exists(path) {
            ItemIcon::File(path.to_owned())
        } else {
            ItemIcon::None
        };
    }

    // (2) The client's own directory, if it named one. A name is used as a *file stem* here, so a
    // client cannot walk out of its own directory with `../`: anything with a separator in it was
    // already refused as a name.
    if let (Some(dir), Some(stem)) = (theme_path, normalize_icon_name(name)) {
        let dir = Path::new(dir);
        if dir.is_absolute() {
            // Flat first, then the one level of theme structure a client-shipped directory
            // realistically has. Deliberately not a full theme walk — this is one app's icons,
            // not an installed theme.
            let roots: [PathBuf; 3] = [
                dir.to_owned(),
                dir.join("hicolor/scalable/apps"),
                dir.join("hicolor/48x48/apps"),
            ];
            for root in roots {
                for ext in ICON_FILE_EXTENSIONS {
                    let candidate = root.join(format!("{stem}.{ext}"));
                    if exists(&candidate) {
                        return ItemIcon::File(candidate);
                    }
                }
            }
        }
    }

    // (3) A plain themed name.
    match normalize_icon_name(name) {
        Some(name) => ItemIcon::Themed(name),
        None => ItemIcon::None,
    }
}

/// Clamp and flatten an untrusted display string, as notification and MPRIS text are.
pub fn clean_text(text: &str) -> String {
    crate::notifications::clamp_text(crate::notifications::flatten_text(text), MAX_TEXT_BYTES)
}

/// One item as the UI sees it: its identity plus its current properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indicator {
    pub item: RegisteredItem,
    pub props: ItemProps,
}

impl Indicator {
    /// Whether this indicator currently takes a slot in the panel: ready, and not `Passive`.
    pub fn is_shown(&self) -> bool {
        self.props.is_ready() && self.props.status.is_visible()
    }
}

/// The indicators the shell knows about, in registration order.
///
/// Ordering is registration order for now — see the open question in
/// `docs/fork/status-notifier-port.md`. It is stable within a session, which is what matters for
/// not having icons swap places under the pointer; across boots it is not, and neither is any
/// other candidate we have (`Id` embeds a pid for most Qt clients).
#[derive(Debug, Default)]
pub struct IndicatorStore {
    items: Vec<Indicator>,
}

impl IndicatorStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace an item's entry. Returns whether anything the UI draws changed.
    pub fn upsert(&mut self, item: RegisteredItem, props: ItemProps) -> bool {
        if let Some(existing) = self.items.iter_mut().find(|i| {
            i.item.unique_name == item.unique_name && i.item.object_path == item.object_path
        }) {
            if existing.item == item && existing.props == props {
                return false;
            }
            let was_shown = existing.is_shown();
            existing.item = item;
            existing.props = props;
            return was_shown || existing.is_shown();
        }

        let indicator = Indicator { item, props };
        let shown = indicator.is_shown();
        self.items.push(indicator);
        shown
    }

    /// Drop an item by its public id. Returns whether the panel changed.
    pub fn remove(&mut self, id: &str) -> bool {
        let Some(idx) = self.items.iter().position(|i| i.item.id == id) else {
            return false;
        };
        self.items.remove(idx).is_shown()
    }

    /// The indicators to draw, in order.
    pub fn shown(&self) -> impl Iterator<Item = &Indicator> {
        self.items.iter().filter(|i| i.is_shown())
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// What the watcher tells the compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusNotifierToSynoik {
    /// An item registered, or its properties changed. Carries the whole state: the watcher
    /// re-reads every property on any `New*` signal, as the extension's proxy does.
    ItemUpdated {
        item: RegisteredItem,
        props: Box<ItemProps>,
    },
    ItemUnregistered {
        id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_argument_is_attributed_to_the_sender() {
        // Ayatana-patched apps send a path and mean "my connection, this path".
        assert_eq!(
            parse_service_argument("/org/ayatana/NotificationItem/foo", Some(":1.42")),
            Ok(ServiceRef::Path {
                unique_name: ":1.42".to_owned(),
                object_path: "/org/ayatana/NotificationItem/foo".to_owned(),
            })
        );
    }

    #[test]
    fn a_bus_name_argument_is_left_for_resolution() {
        // KDE apps send a bus name and mean the default path.
        assert_eq!(
            parse_service_argument("org.kde.StatusNotifierItem-1234-1", None),
            Ok(ServiceRef::Name {
                service: "org.kde.StatusNotifierItem-1234-1".to_owned(),
            })
        );
        // A unique name is a bus name too, and clients do send them.
        assert_eq!(
            parse_service_argument(":1.42", Some(":1.42")),
            Ok(ServiceRef::Name {
                service: ":1.42".to_owned(),
            })
        );
    }

    #[test]
    fn a_path_with_no_sender_is_refused() {
        // Nothing can be registered for a peer we cannot name.
        assert_eq!(
            parse_service_argument("/StatusNotifierItem", None),
            Err(ParseError::NoSender)
        );
    }

    #[test]
    fn nonsense_arguments_are_refused() {
        for arg in ["", "not a name", "no-dots", "//bad//path", "/trailing/"] {
            assert_eq!(
                parse_service_argument(arg, Some(":1.7")),
                Err(ParseError::NotABusNameOrPath),
                "{arg:?} should not parse"
            );
        }
    }

    #[test]
    fn the_id_prefers_the_well_known_name() {
        // A client that registered by well-known name is published under it...
        assert_eq!(
            item_id(
                Some("org.kde.StatusNotifierItem-9-1"),
                ":1.9",
                "/StatusNotifierItem"
            ),
            "org.kde.StatusNotifierItem-9-1"
        );
        // ...but one that registered by path, or by its own unique name, is not: `service ==
        // busName` there, and publishing a bare `:1.9` would collide with every other item on
        // that connection.
        assert_eq!(
            item_id(None, ":1.9", "/org/ayatana/NotificationItem/x"),
            ":1.9@/org/ayatana/NotificationItem/x"
        );
        assert_eq!(
            item_id(Some(":1.9"), ":1.9", "/StatusNotifierItem"),
            ":1.9@/StatusNotifierItem"
        );
    }

    fn item(id: &str, unique_name: &str, object_path: &str) -> RegisteredItem {
        RegisteredItem {
            id: id.to_owned(),
            unique_name: unique_name.to_owned(),
            object_path: object_path.to_owned(),
        }
    }

    #[test]
    fn re_registering_the_same_item_does_not_add_a_second_icon() {
        let mut registry = ItemRegistry::new();
        let it = item("org.kde.Foo", ":1.5", "/StatusNotifierItem");

        assert_eq!(registry.insert(it.clone()), Registration::Added);
        assert_eq!(registry.insert(it), Registration::AlreadyRegistered);
        assert_eq!(registry.ids(), vec!["org.kde.Foo".to_owned()]);
    }

    #[test]
    fn one_connection_may_serve_several_items() {
        let mut registry = ItemRegistry::new();
        registry.insert(item("a", ":1.5", "/one"));
        registry.insert(item("b", ":1.5", "/two"));

        assert_eq!(registry.ids(), vec!["a".to_owned(), "b".to_owned()]);

        // And losing the connection takes both, in one go.
        assert_eq!(
            registry.remove_owner(":1.5"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn the_same_name_on_a_new_connection_is_a_new_item() {
        // A well-known name that moved connections is a restarted app, not the old one: keying on
        // the id would leave the dead item in the list and drop the live one as a duplicate.
        let mut registry = ItemRegistry::new();
        registry.insert(item("org.kde.Foo", ":1.5", "/StatusNotifierItem"));
        assert_eq!(
            registry.insert(item("org.kde.Foo", ":1.9", "/StatusNotifierItem")),
            Registration::Added
        );

        assert_eq!(
            registry.remove_owner(":1.5"),
            vec!["org.kde.Foo".to_owned()]
        );
        assert!(registry.contains_owner(":1.9"));
    }

    #[test]
    fn removing_one_item_leaves_its_siblings() {
        let mut registry = ItemRegistry::new();
        registry.insert(item("a", ":1.5", "/one"));
        registry.insert(item("b", ":1.5", "/two"));

        assert_eq!(registry.remove_item(":1.5", "/one"), Some("a".to_owned()));
        assert_eq!(registry.ids(), vec!["b".to_owned()]);
        assert_eq!(registry.remove_item(":1.5", "/one"), None);
    }

    #[test]
    fn an_icon_name_survives_the_two_common_malformations() {
        // A plain name is left alone.
        assert_eq!(
            normalize_icon_name("nextcloud"),
            Some("nextcloud".to_owned())
        );
        // A file name loses its extension, which is what the lookup wants.
        assert_eq!(normalize_icon_name("foo.png"), Some("foo".to_owned()));
        assert_eq!(normalize_icon_name("foo.svg"), Some("foo".to_owned()));
        // A dot that is not an extension is part of the name (`org.kde.foo` is a real icon name).
        assert_eq!(
            normalize_icon_name("org.kde.foo"),
            Some("org.kde.foo".to_owned())
        );
        // A path is not a name: S3 loads those, and a themed lookup for one finds nothing.
        assert_eq!(normalize_icon_name("/usr/share/pixmaps/foo.png"), None);
        assert_eq!(normalize_icon_name("sub/dir"), None);
        assert_eq!(normalize_icon_name("   "), None);
    }

    #[test]
    fn the_best_pixmap_is_the_smallest_one_big_enough() {
        // Three sizes offered, 16/32/64 px square. Asking for 32 takes the exact one...
        let entries = [
            (16, 16, 16 * 16 * 4),
            (64, 64, 64 * 64 * 4),
            (32, 32, 32 * 32 * 4),
        ];
        assert_eq!(pick_pixmap(&entries, 32), Some(2));
        // ...asking for 24 takes the 32 (downscaling beats upscaling)...
        assert_eq!(pick_pixmap(&entries, 24), Some(2));
        // ...and asking for more than any of them takes the largest.
        assert_eq!(pick_pixmap(&entries, 128), Some(1));
    }

    #[test]
    fn a_pixmap_that_lies_about_its_size_is_not_a_candidate() {
        // Claimed 64x64 but only 16x16 worth of bytes: drawing it would read past the message.
        let entries = [(64, 64, 16 * 16 * 4)];
        assert_eq!(pick_pixmap(&entries, 16), None);
        // Absurd dimensions are refused before any allocation is sized from them.
        let huge = (MAX_PIXMAP_DIM + 1) as i32;
        let entries = [(huge, huge, (huge as usize) * (huge as usize) * 4)];
        assert_eq!(pick_pixmap(&entries, 16), None);
        // As are negative ones, which is what an `i32` on the wire allows.
        assert_eq!(pick_pixmap(&[(-1, -1, 4)], 16), None);
    }

    #[test]
    fn a_pixmap_is_reordered_and_premultiplied() {
        // One opaque red pixel and one half-transparent white one, ARGB in network byte order.
        let argb = [
            0xff, 0xff, 0x00, 0x00, // opaque red
            0x80, 0xff, 0xff, 0xff, // 50% white
        ];
        let pixmap = pixmap_from_argb(2, 1, &argb).expect("valid pixmap");
        assert_eq!(pixmap.width, 2);
        assert_eq!(pixmap.height, 1);
        // R,G,B,A order, and the translucent pixel's colour scaled by its alpha — without the
        // premultiply every soft edge would glow.
        assert_eq!(&pixmap.rgba[0..4], &[0xff, 0x00, 0x00, 0xff]);
        assert_eq!(&pixmap.rgba[4..8], &[0x80, 0x80, 0x80, 0x80]);

        // A short buffer is refused rather than padded.
        assert!(pixmap_from_argb(2, 1, &argb[..7]).is_none());
        assert!(pixmap_from_argb(0, 1, &[]).is_none());
    }

    #[test]
    fn two_pixmaps_with_the_same_pixels_compare_equal() {
        // Equality is by content hash, so an item re-sending an unchanged pixmap is not a change
        // and does not re-upload a texture.
        let argb = [0xff, 0x12, 0x34, 0x56];
        let a = pixmap_from_argb(1, 1, &argb).unwrap();
        let b = pixmap_from_argb(1, 1, &argb).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.hash, b.hash);

        let c = pixmap_from_argb(1, 1, &[0xff, 0x12, 0x34, 0x57]).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn an_absolute_icon_name_is_taken_as_a_file() {
        // `indicator-sensors` sends a path where a name belongs.
        let exists = |p: &std::path::Path| p == std::path::Path::new("/usr/share/pixmaps/x.png");
        assert_eq!(
            icon_from_name("/usr/share/pixmaps/x.png", None, &exists),
            ItemIcon::File("/usr/share/pixmaps/x.png".into())
        );
        // A path that is not there yields nothing — not a themed lookup for a filename.
        assert_eq!(icon_from_name("/nope/x.png", None, &exists), ItemIcon::None);
    }

    #[test]
    fn an_icon_theme_path_is_searched_before_the_installed_themes() {
        let found = std::path::Path::new("/opt/app/icons/tray.svg");
        let exists = move |p: &std::path::Path| p == found;

        // A name plus the client's own directory resolves to the client's file...
        assert_eq!(
            icon_from_name("tray", Some("/opt/app/icons"), &exists),
            ItemIcon::File(found.to_owned())
        );
        // ...including when the client sent the name with an extension, which is the same
        // malformation the themed path has to undo.
        assert_eq!(
            icon_from_name("tray.png", Some("/opt/app/icons"), &exists),
            ItemIcon::File(found.to_owned())
        );
        // A directory that does not have it falls back to the themed name, so an item whose
        // theme path is stale still draws.
        assert_eq!(
            icon_from_name("tray", Some("/opt/other"), &exists),
            ItemIcon::Themed("tray".to_owned())
        );
        // A relative theme path is ignored: it would resolve against our cwd, not the client's.
        assert_eq!(
            icon_from_name("tray", Some("relative/icons"), &exists),
            ItemIcon::Themed("tray".to_owned())
        );
    }

    #[test]
    fn a_theme_path_cannot_be_escaped_with_a_traversal() {
        // Any name containing a separator was already refused as a name, so `../../etc/x` cannot
        // become a lookup outside the directory the client named.
        let exists = |_: &std::path::Path| true;
        assert_eq!(
            icon_from_name("../../etc/passwd", Some("/opt/app/icons"), &exists),
            ItemIcon::None
        );
    }

    #[test]
    fn an_item_is_not_shown_until_it_is_ready_and_active() {
        let mut props = ItemProps::default();
        // Nothing fetched yet: no Id, and Passive by default. Both must keep it out of the panel.
        assert!(!props.is_ready());
        props.status = ItemStatus::Active;
        assert!(!props.is_ready());

        props.app_id = "nextcloud".to_owned();
        assert!(props.is_ready());

        // A menu is *not* required — the extension demands one and so hides activate-only items.
        assert_eq!(props.menu_path, None);
        let indicator = Indicator {
            item: item("x", ":1.1", "/StatusNotifierItem"),
            props: props.clone(),
        };
        assert!(indicator.is_shown());

        props.status = ItemStatus::Passive;
        let indicator = Indicator {
            item: item("x", ":1.1", "/StatusNotifierItem"),
            props,
        };
        assert!(!indicator.is_shown());
    }

    #[test]
    fn the_attention_icon_wins_only_while_asking_for_attention() {
        let themed = |n: &str| ItemIcon::Themed(n.to_owned());
        let mut props = ItemProps {
            app_id: "chat".to_owned(),
            status: ItemStatus::Active,
            icon: themed("chat"),
            attention_icon: themed("chat-urgent"),
            ..ItemProps::default()
        };
        assert_eq!(props.effective_icon(), &themed("chat"));

        props.status = ItemStatus::NeedsAttention;
        assert_eq!(props.effective_icon(), &themed("chat-urgent"));

        // A client that flips the status without providing a second icon keeps the first one,
        // rather than losing its icon at the moment it wants to be noticed.
        props.attention_icon = ItemIcon::None;
        assert_eq!(props.effective_icon(), &themed("chat"));
    }

    #[test]
    fn the_store_reports_only_changes_the_panel_would_show() {
        let mut store = IndicatorStore::new();
        let it = item("a", ":1.1", "/StatusNotifierItem");
        let passive = ItemProps {
            app_id: "a".to_owned(),
            ..ItemProps::default()
        };

        // A passive item is tracked but draws nothing, so adding it changes no pixels.
        assert!(!store.upsert(it.clone(), passive.clone()));
        assert_eq!(store.len(), 1);
        assert_eq!(store.shown().count(), 0);

        // Going Active does.
        let active = ItemProps {
            status: ItemStatus::Active,
            ..passive.clone()
        };
        assert!(store.upsert(it.clone(), active.clone()));
        assert_eq!(store.shown().count(), 1);

        // Re-sending the identical state does not.
        assert!(!store.upsert(it.clone(), active));
        // Going back to Passive does, because something was on screen and now is not.
        assert!(store.upsert(it.clone(), passive));
        assert_eq!(store.shown().count(), 0);
        // And removing a hidden item changes nothing either.
        assert!(!store.remove("a"));
        assert!(store.is_empty());
    }

    #[test]
    fn a_re_registration_may_improve_the_id() {
        // Registering by path first and by well-known name second is the same item under a better
        // name — the list should follow, without gaining an entry.
        let mut registry = ItemRegistry::new();
        registry.insert(item(
            ":1.5@/StatusNotifierItem",
            ":1.5",
            "/StatusNotifierItem",
        ));
        assert_eq!(
            registry.insert(item("org.kde.Foo", ":1.5", "/StatusNotifierItem")),
            Registration::AlreadyRegistered
        );
        assert_eq!(registry.ids(), vec!["org.kde.Foo".to_owned()]);
    }
}
