// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Where an image the shell displays comes from — the plain-data seam between *validating* an
//! app-supplied URI and *loading* it.
//!
//! Album art is the caller: `mpris:artUrl` is a URI chosen by whatever media player is on the bus,
//! and GNOME hands it straight to `Gio.File.new_for_uri` (`js/ui/messageList.js:817-819`), so the
//! shell will read a local path or fetch a remote URL on an arbitrary app's say-so. We do the same,
//! because that is the behaviour being ported — but only after this module has decided the URI is
//! one we are willing to touch, and the decision is expressed as data ([`ImageSource`]) rather than
//! as "we already opened it".
//!
//! Keeping the seam plain-data is what lets the loader move: today the remote fetch runs on a
//! worker thread through gvfs, and the intended endgame is our own Rust transport. Neither side of
//! this module has to know which.
//!
//! **Remote art is off by default.** [`remote_fetch_enabled`] gates it, and nothing we have found
//! yet needs it: both browsers on this machine download the artwork themselves and publish a
//! `file://` path (see `docs/fork/osd-media-port.md`). Fetching means the shell issues a request an
//! arbitrary app on the bus chose the target of — a tracking beacon carrying the user's IP, a
//! channel out of a sandbox with no network permission of its own — and the guards below are
//! best-effort, since gvfs owns the redirect handling. Not worth carrying that on by default for a
//! capability with no known consumer. The code stays, ready for the player that needs it.
//!
//! **What is refused, and why:**
//!
//! - **Any scheme but `file`, `http`, `https`.** GNOME accepts whatever gvfs mounts, which includes
//!   `admin://`, `sftp://`, `dav://`, `archive://` and friends — a much wider surface reachable by
//!   any app on the bus, some of it with the user's stored credentials attached.
//! - **A length cap before any parsing**, so a hostile URI cannot be a denial of service in itself.
//! - **Percent-escapes that do not decode, and any NUL**, which would truncate a path at the
//!   syscall boundary and open a prefix of what was named.
//! - **Remote hosts that resolve to loopback, private, link-local or unspecified addresses**
//!   ([`remote_is_permitted`]) — see its docs for the limit of that check.
//! - **Anything that is not a regular file, and anything over [`MAX_IMAGE_BYTES`]** — a local path
//!   is chosen by an app exactly as freely as a URL is, so `file:///dev/zero` has to be refused for
//!   the same reason a hostile server's endless response is.

use std::net::{IpAddr, ToSocketAddrs as _};
use std::path::PathBuf;

/// Cap on the URI we will even parse.
pub const MAX_URI_BYTES: usize = 4096;

/// How long a remote fetch may take before it is abandoned.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Cap on an image we will load, in bytes. Album art is tens to hundreds of KB; this is generous
/// enough not to reject real covers and small enough that neither a hostile server nor a hostile
/// **path** can make the shell buy an arbitrary amount of memory.
///
/// It applies to local files as much as to fetches: the path is chosen by an app just as freely as
/// the URL is, and `file:///dev/zero` is a URI a player is perfectly able to publish.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Whether remote art may actually be fetched — **off unless `SYNOIK_REMOTE_ART=1`**.
///
/// Deliberately *not* a config or gsettings knob: GNOME has no such setting, and the fork's model
/// is GNOME's rather than a new surface of our own. An env var is the same shape as
/// `SYNOIK_VK_VALIDATION` — a developer switch for a capability that is not on the supported path.
///
/// Checked in the loader rather than in [`ImageSource::from_uri`] so the URI vocabulary stays pure
/// and testable: a remote URL still parses into [`ImageSource::Remote`], it just does not load.
pub fn remote_fetch_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("SYNOIK_REMOTE_ART").is_some_and(|value| value == "1"))
}

/// A place an image can be loaded from, after validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageSource {
    /// An absolute local path.
    File(PathBuf),
    /// An `http(s)` URL, kept as the original string because that is what the transport wants.
    /// Being here means the scheme and length passed; whether the *address* is permitted is
    /// answered at fetch time by [`remote_is_permitted`], since it needs DNS.
    Remote(String),
}

impl ImageSource {
    /// Validate an app-supplied URI. `None` for anything we will not load.
    pub fn from_uri(uri: &str) -> Option<Self> {
        if uri.len() > MAX_URI_BYTES {
            return None;
        }
        if let Some(rest) = uri.strip_prefix("file://") {
            return Self::local(rest);
        }
        // Scheme match is ASCII-case-insensitive (RFC 3986 §3.1); the rest of the URI is handed to
        // the transport verbatim.
        let scheme_end = uri.find("://")?;
        let scheme = &uri[..scheme_end];
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return None;
        }
        // A host is required: `http:///path` has none, and would be resolved by the transport in
        // some way we have not reasoned about.
        let after = &uri[scheme_end + 3..];
        let host = after.split(['/', '?', '#']).next().unwrap_or("");
        if host.is_empty() {
            return None;
        }
        // Credentials in the URI would be sent by the transport on the app's behalf.
        if host.contains('@') {
            return None;
        }
        Some(Self::Remote(uri.to_owned()))
    }

    /// `file:///path` — the authority must be empty or `localhost`, per RFC 8089.
    fn local(rest: &str) -> Option<Self> {
        let path = match rest.strip_prefix("localhost/") {
            Some(path) => format!("/{path}"),
            None if rest.starts_with('/') => rest.to_owned(),
            None => return None,
        };
        let path = PathBuf::from(percent_decode(&path)?);
        path.is_absolute().then_some(Self::File(path))
    }

    /// The `(host, port)` a remote source would connect to, for the address check.
    fn authority(&self) -> Option<(String, u16)> {
        let Self::Remote(url) = self else {
            return None;
        };
        let scheme_end = url.find("://")?;
        let default_port = if url[..scheme_end].eq_ignore_ascii_case("https") {
            443
        } else {
            80
        };
        let after = &url[scheme_end + 3..];
        let authority = after.split(['/', '?', '#']).next().unwrap_or("");
        // A bracketed IPv6 literal carries colons of its own.
        if let Some(rest) = authority.strip_prefix('[') {
            let (host, tail) = rest.split_once(']')?;
            let port = match tail.strip_prefix(':') {
                Some(p) => p.parse().ok()?,
                None => default_port,
            };
            return Some((host.to_owned(), port));
        }
        match authority.split_once(':') {
            Some((host, port)) => Some((host.to_owned(), port.parse().ok()?)),
            None => Some((authority.to_owned(), default_port)),
        }
    }
}

/// Whether a remote source may be fetched: it must resolve, and **every** address it resolves to
/// must be a public one. Blocks the shell being used to probe `localhost` and the LAN on behalf of
/// an app that cannot reach them itself — the case that matters is a sandboxed app with no network
/// permission of its own.
///
/// **Known limit:** this checks the address we were *asked* for, not the one finally connected to.
/// gvfs follows redirects internally and offers no hook to inspect them, so a public URL that
/// redirects to a private address is not caught here. Closing that needs a transport whose redirect
/// handling we own — a reason for the eventual own-transport work, not a blocker for this one.
/// Called on the fetch worker: it does a blocking DNS lookup.
pub fn remote_is_permitted(source: &ImageSource) -> bool {
    let Some((host, port)) = source.authority() else {
        return false;
    };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    let mut any = false;
    for addr in addrs {
        any = true;
        if !is_public(addr.ip()) {
            return false;
        }
    }
    any
}

/// Whether an address is one we are willing to have the shell talk to on an app's behalf.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // `is_shared` (100.64/10, carrier NAT) is still unstable, hence the literal.
            let shared = v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]);
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || shared)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            // Unique-local fc00::/7 and link-local fe80::/10; `is_unique_local` is unstable.
            let unique_local = segments[0] & 0xfe00 == 0xfc00;
            let link_local = segments[0] & 0xffc0 == 0xfe80;
            // An IPv4-mapped address is the v4 rules again, not a free pass.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || unique_local
                || link_local)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_uris_resolve_to_absolute_paths() {
        assert_eq!(
            ImageSource::from_uri("file:///home/u/cover.png"),
            Some(ImageSource::File(PathBuf::from("/home/u/cover.png")))
        );
        assert_eq!(
            ImageSource::from_uri("file://localhost/home/u/cover.png"),
            Some(ImageSource::File(PathBuf::from("/home/u/cover.png")))
        );
        assert_eq!(
            ImageSource::from_uri("file:///tmp/a%20b.png"),
            Some(ImageSource::File(PathBuf::from("/tmp/a b.png")))
        );

        // Neither a bare path nor a relative file URI is a URI we accept.
        assert_eq!(ImageSource::from_uri("/home/u/cover.png"), None);
        assert_eq!(ImageSource::from_uri("file://cover.png"), None);
        // Malformed and truncating escapes.
        assert_eq!(ImageSource::from_uri("file:///tmp/%zz.png"), None);
        assert_eq!(ImageSource::from_uri("file:///tmp/a%00b.png"), None);
    }

    /// `http(s)` is accepted — GNOME hands any URI to gvfs — but the scheme list is a whitelist,
    /// because gvfs also mounts `admin://`, `sftp://` and `dav://`, some of them with the user's
    /// stored credentials, and any app on the bus picks this string.
    #[test]
    fn only_http_https_and_file_are_accepted() {
        for url in [
            "https://i.scdn.co/image/abc",
            "http://example.com/cover.png",
            "HTTPS://EXAMPLE.COM/cover.png",
            "https://example.com:8443/cover.png",
        ] {
            assert_eq!(
                ImageSource::from_uri(url),
                Some(ImageSource::Remote(url.to_owned())),
                "{url} must be accepted"
            );
        }

        for url in [
            "admin:///etc/shadow",
            "sftp://host/cover.png",
            "dav://host/cover.png",
            "archive://x/cover.png",
            "resource:///org/gnome/x.png",
            "data:image/png;base64,AAAA",
            "https:///no-host.png",
            // Credentials would be sent by the transport on the app's behalf.
            "https://user:pw@example.com/cover.png",
            "not a uri at all",
        ] {
            assert_eq!(ImageSource::from_uri(url), None, "{url} must be refused");
        }

        assert_eq!(
            ImageSource::from_uri(&format!("https://example.com/{}", "a".repeat(8192))),
            None,
            "the length cap applies before any parsing"
        );
    }

    /// The authority parse feeds the address check, so a port and an IPv6 literal have to come out
    /// intact — get this wrong and the guard checks the wrong host, or none.
    #[test]
    fn authority_parsing_survives_ports_and_ipv6() {
        let auth = |u: &str| ImageSource::from_uri(u).and_then(|s| s.authority());
        assert_eq!(
            auth("http://example.com/x"),
            Some(("example.com".into(), 80))
        );
        assert_eq!(
            auth("https://example.com/x"),
            Some(("example.com".into(), 443))
        );
        assert_eq!(
            auth("https://example.com:8443/x?y#z"),
            Some(("example.com".into(), 8443))
        );
        assert_eq!(auth("http://[::1]:9000/x"), Some(("::1".into(), 9000)));
        assert_eq!(auth("http://[::1]/x"), Some(("::1".into(), 80)));
    }

    /// The point of the guard: an app naming a private address must not get the shell to connect to
    /// it. Uses literals only, so it never touches DNS or the network.
    #[test]
    fn private_and_loopback_addresses_are_refused() {
        let permitted = |u: &str| remote_is_permitted(&ImageSource::from_uri(u).unwrap());

        for url in [
            "http://127.0.0.1:8080/probe",
            "http://127.1.2.3/probe",
            "http://[::1]:8080/probe",
            "http://10.0.0.5/probe",
            "http://172.16.4.1/probe",
            "http://192.168.1.1/probe",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/probe",
            "http://100.64.1.1/probe",
            "http://[::ffff:127.0.0.1]/probe",
        ] {
            assert!(!permitted(url), "{url} must be refused");
        }

        // A public literal passes the address rule (no DNS involved).
        assert!(permitted("http://93.184.216.34/cover.png"));
    }
}
