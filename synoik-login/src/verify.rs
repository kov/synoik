// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A verification worker: `synoik-login verify SERVICE USER`. One PAM authentication whose
//! questions and messages are relayed to the daemon on fd 3 (`docs/fork/greeter.md` §2.3, §2.5).
//!
//! On success it reports and then **stays**, as GDM's worker does after `VerificationComplete`:
//! the conversation ends when the daemon ends it, and the client hears `ConversationStopped` then.
//!
//! `synoik-login login SERVICE USER -- PROGRAM...` is the login form (§2.4): account checks are
//! enforced, credentials established, and a conversation the daemon picks goes on to open the
//! session and parent `PROGRAM` in it.

use std::os::fd::FromRawFd as _;
use std::os::unix::net::UnixStream;

use crate::ipc::{Frame, Secret};
use crate::pam::{self, Conversation, Pam};
use crate::worker::{self, STATUS_FD};

struct Relay(UnixStream);

impl Relay {
    fn send(&mut self, frame: Frame) -> bool {
        frame.write_to(&mut self.0).is_ok()
    }
}

impl Conversation for Relay {
    fn ask(&mut self, prompt: &str, secret: bool) -> Option<Secret> {
        let question = if secret {
            Frame::SecretInfoQuery(prompt.to_owned())
        } else {
            Frame::InfoQuery(prompt.to_owned())
        };
        if !self.send(question) {
            return None;
        }
        match Frame::read_from(&mut self.0) {
            Ok(Some(Frame::Answer(answer))) => Some(answer),
            _ => None,
        }
    }

    fn info(&mut self, text: &str) {
        self.send(Frame::Info(text.to_owned()));
    }

    fn problem(&mut self, text: &str) {
        self.send(Frame::Problem(text.to_owned()));
    }
}

pub fn run(service: &str, user: &str) -> anyhow::Result<i32> {
    // SAFETY: the daemon put one end of a socket pair here; nothing else owns it.
    let stream = unsafe { UnixStream::from_raw_fd(STATUS_FD) };
    let mut report = Relay(stream.try_clone()?);

    let mut pam = match Pam::start_with(service, user, Box::new(Relay(stream.try_clone()?))) {
        Ok(pam) => pam,
        Err(err) => {
            eprintln!("synoik-login: {service} for {user}: {err:#}");
            report.send(Frame::Unavailable(String::new()));
            return Ok(1);
        }
    };

    let code = pam.authenticate_code();
    if code != pam::PAM_SUCCESS {
        eprintln!("synoik-login: {service} for {user}: {}", pam.strerror(code));
        report.send(refusal(service, user, code));
        return Ok(1);
    }

    // Reauthentication proves who is at the seat; an account problem is not a reason to keep
    // them out of a session that is already open (`gdm-session-worker.c:1487-1492`). An expired
    // password is still changed on the way in.
    if pam.acct_mgmt_code() == pam::PAM_NEW_AUTHTOK_REQD {
        if let Err(err) = pam.chauthtok_expired() {
            eprintln!("synoik-login: {service} for {user}: {err:#}");
            report.send(Frame::Failed(generic_message(service).to_owned()));
            return Ok(1);
        }
    }
    // Refreshes what the session's own credentials hold, e.g. Kerberos tickets; a failure does
    // not undo the authentication (`:1776-1783`).
    if let Err(err) = pam.setcred(pam::PAM_REINITIALIZE_CRED) {
        eprintln!("synoik-login: {service} for {user}: {err:#}");
    }

    eprintln!("synoik-login: {service} verified {user}");
    report.send(Frame::Complete);
    // Until the daemon ends the conversation.
    let _ = Frame::read_from(&mut report.0);
    Ok(0)
}

pub fn run_login(service: &str, user: &str, program: &[String]) -> anyhow::Result<i32> {
    // SAFETY: the daemon put one end of a socket pair here; nothing else owns it. `dup2` left it
    // inheritable, and the session's program must not get it.
    let stream = unsafe {
        libc::fcntl(STATUS_FD, libc::F_SETFD, libc::FD_CLOEXEC);
        UnixStream::from_raw_fd(STATUS_FD)
    };
    let mut report = Relay(stream.try_clone()?);
    let fail = |report: &mut Relay, err: anyhow::Error| {
        eprintln!("synoik-login: {service} for {user}: {err:#}");
        report.send(Frame::Failed(generic_message(service).to_owned()));
        Ok(1)
    };

    // An unknown user must read exactly like a wrong password, so PAM refuses it, not this.
    let account = worker::account(user).ok();

    let mut pam = match Pam::start_with(service, user, Box::new(Relay(stream.try_clone()?))) {
        Ok(pam) => pam,
        Err(err) => {
            eprintln!("synoik-login: {service} for {user}: {err:#}");
            report.send(Frame::Unavailable(String::new()));
            return Ok(1);
        }
    };
    if let Err(err) = worker::put_session_env(&mut pam, "user") {
        return fail(&mut report, err);
    }

    let code = pam.authenticate_code();
    if code != pam::PAM_SUCCESS {
        eprintln!("synoik-login: {service} for {user}: {}", pam.strerror(code));
        report.send(refusal(service, user, code));
        return Ok(1);
    }
    // Logging in, unlike reauthenticating, is refused on an account problem; an expired password
    // is changed on the way in (`gdm-session-worker.c:1470-1485`).
    let code = pam.acct_mgmt_code();
    if code == pam::PAM_NEW_AUTHTOK_REQD {
        if let Err(err) = pam.chauthtok_expired() {
            return fail(&mut report, err);
        }
    } else if code != pam::PAM_SUCCESS {
        eprintln!("synoik-login: {service} for {user}: {}", pam.strerror(code));
        report.send(refusal(service, user, code));
        return Ok(1);
    }
    let Some(account) = account else {
        return fail(&mut report, anyhow::anyhow!("no such user"));
    };
    if let Err(err) = worker::establish_credentials(&mut pam, &account) {
        return fail(&mut report, err);
    }

    eprintln!("synoik-login: {service} verified {user} for a login");
    report.send(Frame::Complete);

    // The daemon names one winner; every other conversation is ended here (EOF), and cleans up.
    if !matches!(Frame::read_from(&mut report.0), Ok(Some(Frame::Open))) {
        let _ = pam.setcred(pam::PAM_DELETE_CRED);
        return Ok(1);
    }
    if let Err(err) = pam.open_session() {
        let _ = pam.setcred(pam::PAM_DELETE_CRED);
        return fail(&mut report, err);
    }
    let id = pam.getenv("XDG_SESSION_ID").unwrap_or_default();
    eprintln!("synoik-login: opened session {id} for {user} ({service})");
    report.send(Frame::Opened(id.clone()));

    if !matches!(Frame::read_from(&mut report.0), Ok(Some(Frame::Start))) {
        eprintln!("synoik-login: session {id} for {user} was not started; closing it");
        if let Err(err) = pam.close_session() {
            eprintln!("synoik-login: {err:#}");
        }
        let _ = pam.setcred(pam::PAM_DELETE_CRED);
        return Ok(1);
    }
    // The greeter is done with this conversation; the session outlives it. Shut the socket down
    // rather than drop it: PAM's conversation still holds a clone.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(report);
    worker::parent_session(pam, &account, program, &id)
}

/// What a failed `pam_authenticate` tells the client, worded as GDM words it
/// (`gdm-session-worker.c:819-877`, `1392-1440`).
fn refusal(service: &str, user: &str, code: libc::c_int) -> Frame {
    match code {
        // The reader would not open for a user who has prints: say so (see [`unavailable_text`]).
        pam::PAM_AUTHINFO_UNAVAIL if service == "synoik-fingerprint" => {
            Frame::Unavailable(unavailable_text(enrolment(user)).to_owned())
        }
        // The service cannot run at all — no reader, a missing module. GDM says nothing.
        pam::PAM_AUTHINFO_UNAVAIL | pam::PAM_MODULE_UNKNOWN => Frame::Unavailable(String::new()),
        pam::PAM_MAXTRIES => Frame::Unavailable(max_tries_message(service).to_owned()),
        pam::PAM_ACCT_EXPIRED | pam::PAM_AUTHTOK_EXPIRED => {
            Frame::Failed("Your account was given a time limit that’s now passed.".to_owned())
        }
        // Everything else, `PAM_USER_UNKNOWN` included: an unknown user reads exactly like a
        // wrong password.
        _ => Frame::Failed(generic_message(service).to_owned()),
    }
}

fn generic_message(service: &str) -> &'static str {
    match service {
        "synoik-password" => "Sorry, password authentication didn’t work. Please try again.",
        "synoik-fingerprint" => "Sorry, fingerprint authentication didn’t work. Please try again.",
        _ => "Sorry, that didn’t work. Please try again.",
    }
}

fn max_tries_message(service: &str) -> &'static str {
    match service {
        "synoik-password" => {
            "You reached the maximum password authentication attempts, please try another method"
        }
        "synoik-fingerprint" => {
            "You reached the maximum fingerprint authentication attempts, please try another method"
        }
        _ => "You reached the maximum authentication attempts, please try another method",
    }
}

/// Whether a user has fingerprints enrolled, as fprintd answers root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Enrolment {
    Enrolled,
    NotEnrolled,
    /// fprintd gone, slow, or saying something else.
    Unknown,
}

/// `FINGERPRINT_SERVICE_PROXY_TIMEOUT` (`js/gdm/util.js:50`): fprintd talks to hardware, and the
/// prompt waits on this answer.
const FPRINTD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// **Divergence: GDM says nothing here** (`ServiceUnavailable` with an empty message,
/// `gdm-session-worker.c:1394-1400`). `pam_fprintd` refuses the same way for a reader that will not
/// open and for a user with no prints, and only the first is worth a word: telling someone with
/// no fingerprints that the reader is unavailable reads as a fault. The client cannot tell them
/// apart from the greeter — fprintd lets only root list another user's prints (`setusername` is
/// `auth_admin_keep`) — so the worker, which is root, asks, and sends the line GNOME would show
/// verbatim for a non-empty message (`_onServiceUnavailable`, `util.js:888-894`).
fn unavailable_text(enrolment: Enrolment) -> &'static str {
    match enrolment {
        Enrolment::Enrolled => "Fingerprint reader unavailable",
        // Silence is GDM's answer, and the safe one when we do not know.
        Enrolment::NotEnrolled | Enrolment::Unknown => "",
    }
}

fn enrolment(user: &str) -> Enrolment {
    let reply = (|| -> zbus::Result<Vec<String>> {
        let conn = zbus::blocking::connection::Builder::system()?
            .method_timeout(FPRINTD_TIMEOUT)
            .build()?;
        let manager = zbus::blocking::Proxy::new(
            &conn,
            "net.reactivated.Fprint",
            "/net/reactivated/Fprint/Manager",
            "net.reactivated.Fprint.Manager",
        )?;
        let device: zbus::zvariant::OwnedObjectPath = manager.call("GetDefaultDevice", &())?;
        let device = zbus::blocking::Proxy::new(
            &conn,
            "net.reactivated.Fprint",
            device,
            "net.reactivated.Fprint.Device",
        )?;
        device.call("ListEnrolledFingers", &(user,))
    })();
    let error_name = match &reply {
        Err(zbus::Error::MethodError(name, _, _)) => Some(name.as_str()),
        _ => None,
    };
    let enrolment = enrolment_of(reply.as_deref().map_err(|_| error_name));
    if let (Err(err), Enrolment::Unknown) = (&reply, enrolment) {
        eprintln!("synoik-login: fingerprints of {user}: {err}");
    }
    enrolment
}

/// fprintd says "none" with an error, `NoEnrolledPrints`, rather than an empty list.
fn enrolment_of(reply: Result<&[String], Option<&str>>) -> Enrolment {
    match reply {
        Ok([]) => Enrolment::NotEnrolled,
        Ok(_) => Enrolment::Enrolled,
        Err(Some("net.reactivated.Fprint.Error.NoEnrolledPrints")) => Enrolment::NotEnrolled,
        Err(_) => Enrolment::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_enrolled_user_hears_the_reader_is_unavailable() {
        let fingers = ["right-index-finger".to_owned()];
        assert_eq!(enrolment_of(Ok(&fingers)), Enrolment::Enrolled);
        assert_eq!(enrolment_of(Ok(&[])), Enrolment::NotEnrolled);
        assert_eq!(
            enrolment_of(Err(Some("net.reactivated.Fprint.Error.NoEnrolledPrints"))),
            Enrolment::NotEnrolled
        );
        assert_eq!(
            enrolment_of(Err(Some("org.freedesktop.DBus.Error.ServiceUnknown"))),
            Enrolment::Unknown
        );
        assert_eq!(enrolment_of(Err(None)), Enrolment::Unknown);

        assert_eq!(
            unavailable_text(Enrolment::Enrolled),
            "Fingerprint reader unavailable"
        );
        assert_eq!(unavailable_text(Enrolment::NotEnrolled), "");
        assert_eq!(unavailable_text(Enrolment::Unknown), "");
    }
}
