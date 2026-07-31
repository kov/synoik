//! The MPRIS model: the media players the shell shows a card for, ported from
//! gnome-shell 50.1's `MprisSource`/`MprisPlayer` (`js/ui/mpris.js`).
//!
//! This is the fork-owned store behind the media cards at the top of the message list
//! (`js/ui/messageList.js:1780-1784`); it is fed by the session-bus watcher in
//! `src/dbus/mpris.rs`, which discovers every `org.mpris.MediaPlayer2.*` name.
//!
//! **Untrusted-content seam.** Track titles, artists and `Identity` are strings a *media player*
//! chose, and `mpris:artUrl` is a URI it chose, which gnome-shell hands straight to
//! `Gio.File.new_for_uri` (`messageList.js:817-820`). Everything crossing into this model is
//! therefore plain, validated, bounded data, sanitized on the watcher's side of the channel, so
//! the watcher can be lifted into its own process later. Two divergences fall out of that:
//!
//! - **Art is `file://` only.** A player that publishes `http(s)` cover art — Spotify does — gets
//!   the generic icon instead. GNOME would let gvfs fetch the URL, i.e. let any app on the bus make
//!   the shell issue a network request of its choosing.
//! - Every display string is newline-flattened and byte-capped, as notification text is.
//!
//! The spec-validation of `Metadata` is GNOME's own (`mpris.js:129-165`): players do send faulty
//! metadata, so each field is type-checked with a fallback rather than trusted.

use std::path::PathBuf;

use crate::app_system::AppEntry;
use crate::notifications::{clamp_text, flatten_text};

/// `MPRIS_PLAYER_PREFIX` (`js/ui/mpris.js:18`).
pub const MPRIS_PLAYER_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// The one object every player exposes both interfaces on (`mpris.js:35,38`).
pub const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// Cap for one untrusted display string. Same order as the notification caps: enough for any real
/// title, small enough that a hostile player cannot buy a multi-megabyte glyph run.
const MAX_TEXT_BYTES: usize = 1024;

/// Cap for the artist list, joined into one line by the card (`messageList.js:828`).
const MAX_ARTISTS: usize = 16;

/// Cap on the art URI we will even parse.
const MAX_URI_BYTES: usize = 4096;

/// `PlaybackStatus`. Only `Playing` is distinguished by the UI — it picks the pause icon
/// (`messageList.js:831-835`) — but the tri-state is what the spec defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    pub fn parse(value: &str) -> Self {
        match value {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            _ => Self::Stopped,
        }
    }

    pub fn is_playing(self) -> bool {
        self == Self::Playing
    }
}

/// One `Metadata` entry as it arrived on the bus, reduced to what validation needs: the value if
/// it had one of the spec's types, or the signature it actually had so the mismatch can be logged
/// the way gnome-shell logs it (`mpris.js:140-142`).
#[derive(Debug, Clone, PartialEq)]
pub enum MetaField {
    Str(String),
    Strings(Vec<String>),
    /// Present, but not a type the spec allows here. Carries the D-Bus signature, for the log.
    Malformed(String),
}

/// The three `Metadata` keys the shell consumes (`mpris.js:136,147,157`), each absent, well-typed
/// or malformed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawMetadata {
    /// `xesam:title`
    pub title: Option<MetaField>,
    /// `xesam:artist`
    pub artists: Option<MetaField>,
    /// `mpris:artUrl`
    pub art_url: Option<MetaField>,
}

/// A player's state after validation — the plain data that crosses the seam.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerState {
    /// `org.mpris.MediaPlayer2.Identity`, the fallback source name when there is no app
    /// (`mpris.js:175`).
    pub identity: String,
    /// `DesktopEntry`, which the compositor resolves through the app system. Stored without the
    /// `.desktop` suffix, as it arrives.
    pub desktop_entry: Option<String>,
    /// `CanPlay` — a player is shown **only while this is true** (`mpris.js:179-184,217-223`).
    pub can_play: bool,
    /// `CanRaise`, the fallback way to raise a player with no resolvable app (`mpris.js:98-99`).
    pub can_raise: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub status: PlaybackStatus,
    /// `xesam:title`, validated.
    pub title: String,
    /// `xesam:artist`, validated. Never empty: the fallback is one "Unknown artist" entry.
    pub artists: Vec<String>,
    /// `mpris:artUrl` resolved to a local path, or `None` for absent, malformed or non-`file://`
    /// art. Not read here — the card loads it.
    pub art: Option<PathBuf>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            identity: String::new(),
            desktop_entry: None,
            can_play: false,
            can_raise: false,
            can_go_next: false,
            can_go_previous: false,
            status: PlaybackStatus::Stopped,
            title: unknown_title(),
            artists: vec![unknown_artist()],
            art: None,
        }
    }
}

fn unknown_title() -> String {
    "Unknown title".to_owned()
}

fn unknown_artist() -> String {
    "Unknown artist".to_owned()
}

/// `_updateState`'s metadata validation (`mpris.js:129-165`): each field is type-checked against
/// the spec and falls back rather than propagating a player's mistake. Ours also flattens and
/// caps, per the seam rule.
///
/// Returns the validated triple plus the mismatches worth logging, so the watcher can log them
/// with the bus name attached, as gnome-shell does.
pub fn validate_metadata(raw: &RawMetadata) -> (String, Vec<String>, Option<PathBuf>, Vec<String>) {
    let mut faults = Vec::new();

    let title = match &raw.title {
        Some(MetaField::Str(title)) => clamp_text(flatten_text(title), MAX_TEXT_BYTES),
        Some(other) => {
            faults.push(format!(
                "expected a string title, got {}",
                other.signature()
            ));
            unknown_title()
        }
        None => unknown_title(),
    };

    // The spec's type is `as`. GNOME accepts nothing else, not even a bare string.
    let artists = match &raw.artists {
        Some(MetaField::Strings(artists)) if !artists.is_empty() => artists
            .iter()
            .take(MAX_ARTISTS)
            .map(|artist| clamp_text(flatten_text(artist), MAX_TEXT_BYTES))
            .collect(),
        // An empty `as` is well-typed, so GNOME keeps it and the card's body is blank. Ours does
        // the same rather than inventing an artist.
        Some(MetaField::Strings(_)) => Vec::new(),
        Some(other) => {
            faults.push(format!(
                "expected an array of string artists, got {}",
                other.signature()
            ));
            vec![unknown_artist()]
        }
        None => vec![unknown_artist()],
    };

    let art = match &raw.art_url {
        Some(MetaField::Str(url)) => art_path(url),
        Some(other) => {
            faults.push(format!(
                "expected a string artUrl, got {}",
                other.signature()
            ));
            None
        }
        None => None,
    };

    (title, artists, art, faults)
}

impl MetaField {
    /// What to name this value's type in a fault log.
    fn signature(&self) -> &str {
        match self {
            Self::Str(_) => "s",
            Self::Strings(_) => "as",
            Self::Malformed(signature) => signature,
        }
    }
}

/// Resolve `mpris:artUrl` to a path we are willing to read.
///
/// **`file://` only** (divergence, see the module docs). Also: absolute paths only, and a length
/// cap before any parsing. Percent-escapes are decoded because that is how a URI spells a space.
pub fn art_path(url: &str) -> Option<PathBuf> {
    if url.len() > MAX_URI_BYTES {
        return None;
    }
    // `file:///path` — the authority must be empty or `localhost`, per RFC 8089.
    let rest = url.strip_prefix("file://")?;
    let path = match rest.strip_prefix("localhost/") {
        Some(path) => format!("/{path}"),
        None if rest.starts_with('/') => rest.to_owned(),
        None => return None,
    };

    let path = percent_decode(&path)?;
    let path = PathBuf::from(path);
    path.is_absolute().then_some(path)
}

/// Decode the percent-escapes in a URI path. `None` when an escape is malformed or the result is
/// not UTF-8 — a path we cannot name is a path we will not open.
fn percent_decode(path: &str) -> Option<String> {
    if !path.contains('%') {
        return Some(path.to_owned());
    }

    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hex = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }

    // A NUL would truncate the path at the syscall boundary; refuse rather than open a prefix.
    let decoded = String::from_utf8(out).ok()?;
    (!decoded.contains('\0')).then_some(decoded)
}

/// An update pushed from the watcher to the compositor. Defined here, not in the feature-gated
/// `dbus::mpris`, so `Niri` can name it unconditionally.
#[derive(Debug, Clone, PartialEq)]
pub enum MprisToNiri {
    /// A player appeared or changed. Carries the whole state: the watcher re-reads every property
    /// on any `PropertiesChanged`, as gnome-shell's `_updateState` does.
    PlayerUpdated {
        bus_name: String,
        state: Box<PlayerState>,
    },
    /// The player's bus name lost its owner (`mpris.js:242-249`).
    PlayerRemoved { bus_name: String },
}

/// A request from the compositor to the watcher — the card's controls (`mpris.js:73-100`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NiriToMpris {
    PlayPause(String),
    Next(String),
    Previous(String),
    /// `Raise()`. Only sent when the app could not be raised locally: gnome-shell prefers
    /// activating the app, because a remote `Raise` runs into focus-stealing prevention
    /// (`mpris.js:93-100`).
    Raise(String),
}

/// One tracked player: what the bus said, plus what the compositor resolved from it.
#[derive(Debug, Clone)]
pub struct MprisPlayer {
    pub bus_name: String,
    pub state: PlayerState,
    /// `DesktopEntry` resolved through the app system (`mpris.js:167-172`). Supplies the card's
    /// source name and icon, and is what `raise()` activates.
    pub app: Option<AppEntry>,
}

impl MprisPlayer {
    /// The card's header title: the app's name, falling back to `Identity` (`mpris.js:175`).
    pub fn source_name(&self) -> &str {
        match &self.app {
            Some(app) => &app.name,
            None => &self.state.identity,
        }
    }

    /// The card's body: the artists joined with `', '` (`messageList.js:828`).
    pub fn artists_line(&self) -> String {
        self.state.artists.join(", ")
    }
}

/// `MprisSource` (`mpris.js:189-254`): every `org.mpris.MediaPlayer2.*` name on the bus, in
/// discovery order.
///
/// A player is *tracked* from the moment its name appears but is only *shown* while `CanPlay`
/// (`mpris.js:217-223` turns `notify::can-play` into player-added/removed), so the store keeps
/// both and [`visible`](Self::visible) is what the message list renders.
#[derive(Debug, Clone, Default)]
pub struct MprisStore {
    players: Vec<MprisPlayer>,
}

impl MprisStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every tracked player, including the ones with no `CanPlay`.
    pub fn tracked(&self) -> &[MprisPlayer] {
        &self.players
    }

    /// The players the message list shows a card for, in discovery order — the caller reverses it
    /// where GNOME's insert-at-0 would (`messageList.js:1780-1784`).
    pub fn visible(&self) -> impl Iterator<Item = &MprisPlayer> {
        self.players.iter().filter(|p| p.state.can_play)
    }

    pub fn get(&self, bus_name: &str) -> Option<&MprisPlayer> {
        self.players.iter().find(|p| p.bus_name == bus_name)
    }

    /// Apply one watcher update. `app` is the resolved `DesktopEntry`, which only the compositor
    /// can look up. Returns whether anything the UI renders changed.
    pub fn update(&mut self, bus_name: String, state: PlayerState, app: Option<AppEntry>) -> bool {
        let changed_app = |old: &Option<AppEntry>| match (old, &app) {
            (Some(old), Some(new)) => old.id != new.id,
            (None, None) => false,
            _ => true,
        };

        match self.players.iter_mut().find(|p| p.bus_name == bus_name) {
            Some(player) => {
                if player.state == state && !changed_app(&player.app) {
                    return false;
                }
                // A player that was never shown and still cannot play changed nothing visible.
                let was_visible = player.state.can_play;
                player.state = state;
                player.app = app;
                was_visible || player.state.can_play
            }
            None => {
                let visible = state.can_play;
                self.players.push(MprisPlayer {
                    bus_name,
                    state,
                    app,
                });
                visible
            }
        }
    }

    /// The player's name lost its owner. Returns whether the UI must change.
    pub fn remove(&mut self, bus_name: &str) -> bool {
        let Some(index) = self.players.iter().position(|p| p.bus_name == bus_name) else {
            return false;
        };
        self.players.remove(index).state.can_play
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faulty_metadata_falls_back_per_field() {
        // The spec's types, straight through.
        let good = RawMetadata {
            title: Some(MetaField::Str("Blue in Green".into())),
            artists: Some(MetaField::Strings(vec![
                "Miles Davis".into(),
                "Bill Evans".into(),
            ])),
            art_url: Some(MetaField::Str("file:///tmp/cover%20art.png".into())),
        };
        let (title, artists, art, faults) = validate_metadata(&good);
        assert_eq!(title, "Blue in Green");
        assert_eq!(artists, ["Miles Davis", "Bill Evans"]);
        assert_eq!(art, Some(PathBuf::from("/tmp/cover art.png")));
        assert!(faults.is_empty());

        // A player sending the artist as a bare string (a real-world bug) gets the fallback, and
        // one fault logged -- but its title still comes through: the fields are independent.
        let faulty = RawMetadata {
            title: Some(MetaField::Str("So What".into())),
            artists: Some(MetaField::Str("Miles Davis".into())),
            art_url: Some(MetaField::Malformed("ay".into())),
        };
        let (title, artists, art, faults) = validate_metadata(&faulty);
        assert_eq!(title, "So What");
        assert_eq!(artists, ["Unknown artist"]);
        assert_eq!(art, None);
        assert_eq!(faults.len(), 2, "one per faulty field: {faults:?}");

        // Absent is not faulty -- it is the common case for a player between tracks.
        let (title, artists, art, faults) = validate_metadata(&RawMetadata::default());
        assert_eq!(title, "Unknown title");
        assert_eq!(artists, ["Unknown artist"]);
        assert_eq!(art, None);
        assert!(faults.is_empty());

        // A well-typed but empty artist list is kept: the card's body is simply blank.
        let empty = RawMetadata {
            artists: Some(MetaField::Strings(Vec::new())),
            ..RawMetadata::default()
        };
        assert!(validate_metadata(&empty).1.is_empty());
    }

    /// Untrusted text never reaches the glyph pipeline unflattened or unbounded.
    #[test]
    fn track_text_is_flattened_and_capped() {
        let long = "x".repeat(MAX_TEXT_BYTES * 2);
        let raw = RawMetadata {
            title: Some(MetaField::Str(format!("two\nlines {long}"))),
            artists: Some(MetaField::Strings(vec!["a\nb".into()])),
            ..RawMetadata::default()
        };
        let (title, artists, ..) = validate_metadata(&raw);
        assert!(!title.contains('\n'));
        assert_eq!(title.len(), MAX_TEXT_BYTES);
        assert_eq!(artists, ["a b"]);
    }

    /// Cover art is the one field that is a *capability*, not text: GNOME hands the URI to gvfs,
    /// so any app on the bus could make the shell fetch a URL of its choosing. Ours opens local
    /// files only.
    #[test]
    fn only_local_cover_art_is_accepted() {
        assert_eq!(
            art_path("file:///home/u/cover.png"),
            Some(PathBuf::from("/home/u/cover.png"))
        );
        assert_eq!(
            art_path("file://localhost/home/u/cover.png"),
            Some(PathBuf::from("/home/u/cover.png"))
        );

        // Spotify's art is https -- the documented divergence, and the reason this is a whitelist.
        assert_eq!(art_path("https://i.scdn.co/image/abc"), None);
        assert_eq!(art_path("http://127.0.0.1:1/probe"), None);
        // Neither a bare path nor a relative file URI is a URI we accept.
        assert_eq!(art_path("/home/u/cover.png"), None);
        assert_eq!(art_path("file://cover.png"), None);
        // Malformed and truncating escapes.
        assert_eq!(art_path("file:///tmp/%zz.png"), None);
        assert_eq!(art_path("file:///tmp/a%00b.png"), None);
        assert_eq!(art_path(&format!("file:///tmp/{}", "a".repeat(8192))), None);
    }

    fn state(can_play: bool, title: &str) -> PlayerState {
        PlayerState {
            identity: "Player".into(),
            can_play,
            title: title.into(),
            ..PlayerState::default()
        }
    }

    /// A player is tracked from the moment its name appears, but shown only while `CanPlay`
    /// (`mpris.js:217-223`), and discovery order is the store's order.
    #[test]
    fn only_can_play_players_are_shown() {
        let mut store = MprisStore::new();
        let a = "org.mpris.MediaPlayer2.rhythmbox".to_owned();
        let b = "org.mpris.MediaPlayer2.vlc".to_owned();

        // Appearing without CanPlay is tracked but shows nothing, so it must not redraw.
        assert!(!store.update(a.clone(), state(false, "a"), None));
        assert_eq!(store.tracked().len(), 1);
        assert_eq!(store.visible().count(), 0);

        // Gaining CanPlay is what "player-added" means.
        assert!(store.update(a.clone(), state(true, "a"), None));
        assert_eq!(store.visible().count(), 1);

        assert!(store.update(b.clone(), state(true, "b"), None));
        let names: Vec<_> = store.visible().map(|p| p.bus_name.as_str()).collect();
        assert_eq!(names, [a.as_str(), b.as_str()], "discovery order");

        // An identical update changes nothing.
        assert!(!store.update(b.clone(), state(true, "b"), None));

        // Losing CanPlay is "player-removed", but the player stays tracked -- it can come back.
        assert!(store.update(b.clone(), state(false, "b"), None));
        assert_eq!(store.visible().count(), 1);
        assert_eq!(store.tracked().len(), 2);

        // Removing a hidden player is not a visible change; removing a shown one is.
        assert!(!store.remove(&b));
        assert!(store.remove(&a));
        assert!(!store.remove(&a), "removing what is gone is a no-op");
        assert_eq!(store.tracked().len(), 0);
    }
}
