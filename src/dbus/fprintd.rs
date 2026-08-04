// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Is there a fingerprint reader, and what shape is it? — `net.reactivated.Fprint`.
//!
//! **This is a hardware probe, not a setting.** `enable-fingerprint-authentication` only decides
//! whether to *look*; everything downstream gates on a reader having actually been found
//! (`serviceIsFingerprint`, `js/gdm/util.js:616-619`). GNOME asks fprintd's manager for
//! `GetDefaultDevice` and reads that device's `scan-type` (`_initFingerprintManager`,
//! `util.js:343-387`, `_updateFingerprintReaderType`, `:429-443`).
//!
//! We need the answer for two things, and only those: whether to start `gdm-fingerprint` beside the
//! password conversation at all, and which of two hint strings to show. No scanning, claiming or
//! enrolling happens here — pam_fprintd does all of that inside gdm's worker, which is the whole
//! point of authenticating through the reauthentication channel rather than in this process.
//!
//! # Failing quietly is most of the job
//!
//! On a machine with no reader — the common case — every step of this fails, and none of those
//! failures is worth a word in the journal: fprintd may not be installed (the name is not
//! activatable), or installed with nothing plugged in (`NoSuchDevice`). GNOME enumerates exactly
//! those two as silent (`_handleFingerprintError`, `util.js:400-415`) and logs anything else. So
//! does this. The reader type is `None` either way, and `None` means the second service is never
//! started, so a wrong answer here is inert rather than dangerous.

use std::time::Duration;

const FPRINT: &str = "net.reactivated.Fprint";
const MANAGER_PATH: &str = "/net/reactivated/Fprint/Manager";
const MANAGER_IFACE: &str = "net.reactivated.Fprint.Manager";
const DEVICE_IFACE: &str = "net.reactivated.Fprint.Device";

/// `FINGERPRINT_SERVICE_PROXY_TIMEOUT` (`util.js:50`).
///
/// GNOME's comment says why: "Do not wait too much for fprintd to reply, as in case it hangs we
/// should fail early without having the shell to misbehave". Activating fprintd can spin up a
/// service that talks to hardware, and the lock screen is on screen while it does.
const PROBE_TIMEOUT: Duration = Duration::from_millis(5000);

/// `FingerprintReaderType` (`util.js:65-69`) — the reader's `scan-type`, or none found.
///
/// The two shapes are not cosmetic: they pick which hint the user is given, and telling somebody to
/// swipe a press-only sensor is an instruction that cannot be followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReaderType {
    /// No reader, fprintd absent, or it would not answer. **The default**, and the one that keeps
    /// `gdm-fingerprint` from ever being started.
    #[default]
    None,
    /// `press` — hold a finger on the sensor.
    Press,
    /// `swipe` — draw a finger across it.
    Swipe,
}

impl ReaderType {
    pub fn is_present(self) -> bool {
        self != Self::None
    }

    /// The hint shown under the entry (`util.js:731-746`).
    ///
    /// GNOME **throws fprintd's own `Info` text away** and substitutes this, because the
    /// fingerprint service is not the foreground one and its messages would otherwise read as
    /// instructions for the password prompt. The parenthetical is GNOME's: it is an aside to
    /// "Password:", not a prompt of its own.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Press => Some("(or place finger on reader)"),
            Self::Swipe => Some("(or swipe finger across reader)"),
        }
    }
}

/// Ask fprintd for the default device's scan type.
///
/// Every failure path is [`ReaderType::None`]; see the module docs for which ones are silent.
pub async fn detect(conn: &zbus::Connection) -> ReaderType {
    let manager = match zbus::Proxy::new(conn, FPRINT, MANAGER_PATH, MANAGER_IFACE).await {
        Ok(proxy) => proxy,
        Err(err) => {
            report(&err);
            return ReaderType::None;
        }
    };

    let device = match with_timeout(manager.call_method("GetDefaultDevice", &())).await {
        Some(Ok(reply)) => match reply
            .body()
            .deserialize::<zbus::zvariant::OwnedObjectPath>()
        {
            Ok(path) => path,
            Err(err) => {
                report(&err);
                return ReaderType::None;
            }
        },
        Some(Err(err)) => {
            report(&err);
            return ReaderType::None;
        }
        None => {
            warn!("fprintd did not answer GetDefaultDevice within {PROBE_TIMEOUT:?}");
            return ReaderType::None;
        }
    };

    let device = match zbus::Proxy::new(conn, FPRINT, device, DEVICE_IFACE).await {
        Ok(proxy) => proxy,
        Err(err) => {
            report(&err);
            return ReaderType::None;
        }
    };
    match with_timeout(device.get_property::<String>("scan-type")).await {
        Some(Ok(scan_type)) => parse_scan_type(&scan_type),
        Some(Err(err)) => {
            report(&err);
            ReaderType::None
        }
        None => {
            warn!("fprintd did not answer scan-type within {PROBE_TIMEOUT:?}");
            ReaderType::None
        }
    }
}

/// `FingerprintReaderType[scanType.toUpperCase()]` (`_setFingerprintReaderType`,
/// `util.js:445-451`).
///
/// GNOME *throws* on a value it does not know, which `_handleFingerprintError` then catches into
/// `NONE`. Same destination, without the round trip.
fn parse_scan_type(scan_type: &str) -> ReaderType {
    match scan_type.to_ascii_lowercase().as_str() {
        "press" => ReaderType::Press,
        "swipe" => ReaderType::Swipe,
        other => {
            warn!("fprintd reports an unknown scan-type {other:?}; not offering fingerprint");
            ReaderType::None
        }
    }
}

/// Whether an error is one of the two "there is simply no reader here" cases GNOME passes over in
/// silence (`util.js:404-412`), rather than something worth a journal line.
fn report(err: &impl std::fmt::Debug) {
    let text = format!("{err:?}");
    // `ServiceUnknown` is fprintd not being installed at all; `NoSuchDevice` is it being installed
    // with nothing attached. Matched on the wire error names, which are what both zbus and GNOME
    // see — the human-readable half is translated and cannot be matched on.
    if text.contains("ServiceUnknown")
        || text.contains("NameHasNoOwner")
        || text.contains("NoSuchDevice")
    {
        debug!("no fingerprint reader: {text}");
        return;
    }
    warn!("error talking to fprintd: {text}");
}

/// Run `fut`, giving up after [`PROBE_TIMEOUT`]. `None` is the timeout.
async fn with_timeout<T>(fut: impl std::future::Future<Output = T>) -> Option<T> {
    use futures_util::future::{select, Either};

    let timeout = async_io::Timer::after(PROBE_TIMEOUT);
    futures_util::pin_mut!(fut);
    match select(fut, timeout).await {
        Either::Left((value, _)) => Some(value),
        Either::Right(_) => None,
    }
}

/// Probe once, on a connection of our own, and send the answer to the main loop.
///
/// Once rather than watched: GNOME re-probes when the fingerprint setting is turned on or the
/// default service changes (`_updateEnabledServices`, `util.js:627-637`), but a reader appearing
/// mid-session is a USB hotplug we have no signal for either way — fprintd's manager has no
/// device-added signal to subscribe to. The probe costs one round trip at startup and the answer
/// is inert when it is `None`, which is the common case.
pub fn start(
    to_niri: calloop::channel::Sender<ReaderType>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::system()?;
    let async_conn = conn.inner().clone();
    conn.inner()
        .executor()
        .spawn(
            async move {
                let _ = to_niri.send(detect(&async_conn).await);
            },
            "probe fprintd",
        )
        .detach();
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scan type decides which instruction the user is given, so an unknown value must not
    /// guess.
    ///
    /// Telling somebody to swipe a press-only sensor is an instruction that cannot be followed, and
    /// they have no way to know the shell is wrong rather than the hardware — they just keep
    /// trying. A reader we cannot describe is therefore not offered at all, which is also what
    /// GNOME's throw into `_handleFingerprintError` amounts to.
    #[test]
    fn an_unknown_scan_type_offers_no_fingerprint() {
        assert_eq!(parse_scan_type("press"), ReaderType::Press);
        assert_eq!(parse_scan_type("swipe"), ReaderType::Swipe);
        // fprintd's property is lowercase; GNOME upper-cases before its lookup, so match either.
        assert_eq!(parse_scan_type("PRESS"), ReaderType::Press);
        assert_eq!(parse_scan_type("something-new"), ReaderType::None);
        assert_eq!(parse_scan_type(""), ReaderType::None);

        // And nothing without a reader ever starts the service or shows a hint.
        assert!(!ReaderType::None.is_present());
        assert_eq!(ReaderType::None.hint(), None);
        assert!(ReaderType::Press.is_present());
        assert_ne!(ReaderType::Press.hint(), ReaderType::Swipe.hint());
    }
}
