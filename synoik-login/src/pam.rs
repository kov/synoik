// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A thin binding of the dozen libpam calls a worker makes (`security/pam_appl.h`).
//!
//! Ours rather than a crate's so the conversation's memory and the handle's lifetime across the
//! session fork stay explicit (`docs/fork/greeter.md` §1).

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;

use anyhow::bail;

use crate::ipc::{wipe, Secret};

pub const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
pub const PAM_AUTHINFO_UNAVAIL: c_int = 9;
pub const PAM_MAXTRIES: c_int = 11;
pub const PAM_NEW_AUTHTOK_REQD: c_int = 12;
pub const PAM_ACCT_EXPIRED: c_int = 13;
const PAM_CONV_ERR: c_int = 19;
pub const PAM_AUTHTOK_EXPIRED: c_int = 27;
pub const PAM_MODULE_UNKNOWN: c_int = 28;

pub const PAM_ESTABLISH_CRED: c_int = 0x0002;
pub const PAM_DELETE_CRED: c_int = 0x0004;
pub const PAM_REINITIALIZE_CRED: c_int = 0x0008;
pub const PAM_CHANGE_EXPIRED_AUTHTOK: c_int = 0x0020;
pub const PAM_DATA_SILENT: c_int = 0x4000_0000;

const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;
/// `PAM_MAX_RESP_SIZE`: longer answers are cut, as GDM cuts them.
const MAX_RESP_SIZE: usize = 512;

/// The other side of a PAM conversation.
pub trait Conversation {
    /// A question; `None` abandons the conversation.
    fn ask(&mut self, prompt: &str, secret: bool) -> Option<Secret>;
    fn info(&mut self, text: &str);
    fn problem(&mut self, text: &str);
}

#[repr(C)]
struct PamHandle {
    _private: [u8; 0],
}

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: ConvFn,
    appdata: *mut c_void,
}

#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conv: *const PamConv,
        handle: *mut *mut PamHandle,
    ) -> c_int;
    fn pam_end(handle: *mut PamHandle, status: c_int) -> c_int;
    fn pam_authenticate(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_chauthtok(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_setcred(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_open_session(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_close_session(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_putenv(handle: *mut PamHandle, name_value: *const c_char) -> c_int;
    fn pam_set_item(handle: *mut PamHandle, item_type: c_int, item: *const libc::c_void) -> c_int;
    fn pam_getenv(handle: *mut PamHandle, name: *const c_char) -> *const c_char;
    fn pam_getenvlist(handle: *mut PamHandle) -> *mut *mut c_char;
    fn pam_strerror(handle: *mut PamHandle, errnum: c_int) -> *const c_char;
}

/// Answers no question: the services a worker runs without a client (the greeter's own,
/// autologin) prompt for nothing, and a module that asks anyway fails the conversation.
unsafe extern "C" fn refuse_all(
    _num_msg: c_int,
    _msg: *mut *const PamMessage,
    _resp: *mut *mut PamResponse,
    _appdata: *mut c_void,
) -> c_int {
    PAM_CONV_ERR
}

/// Relays every message to the `Conversation` behind `appdata`.
unsafe extern "C" fn relay(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata.is_null() {
        return PAM_CONV_ERR;
    }
    let n = num_msg as usize;
    // SAFETY: `appdata` is the `Box<dyn Conversation>` `Pam::start_with` registered, alive for
    // the transaction.
    let conv = unsafe { &mut *appdata.cast::<Box<dyn Conversation>>() };
    // SAFETY: calloc'd, as PAM frees it with free().
    let replies =
        unsafe { libc::calloc(n, std::mem::size_of::<PamResponse>()) }.cast::<PamResponse>();
    if replies.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..n {
        // SAFETY: Linux-PAM passes an array of `num_msg` message pointers.
        let message = unsafe { &**msg.add(i) };
        let text = if message.msg.is_null() {
            String::new()
        } else {
            // SAFETY: a NUL-terminated string PAM owns for the call.
            unsafe { CStr::from_ptr(message.msg) }
                .to_string_lossy()
                .into_owned()
        };
        match message.msg_style {
            PAM_PROMPT_ECHO_OFF | PAM_PROMPT_ECHO_ON => {
                let Some(answer) = conv.ask(&text, message.msg_style == PAM_PROMPT_ECHO_OFF) else {
                    // SAFETY: the array and the answers so far are ours to wipe and free.
                    unsafe { free_replies(replies, n) };
                    return PAM_CONV_ERR;
                };
                let bytes = answer.as_bytes();
                let len = bytes.len().min(MAX_RESP_SIZE - 1);
                // SAFETY: malloc'd for PAM to free; `len + 1` bytes, NUL-terminated.
                unsafe {
                    let copy = libc::malloc(len + 1).cast::<u8>();
                    if copy.is_null() {
                        free_replies(replies, n);
                        return PAM_BUF_ERR;
                    }
                    ptr::copy_nonoverlapping(bytes.as_ptr(), copy, len);
                    *copy.add(len) = 0;
                    (*replies.add(i)).resp = copy.cast();
                }
            }
            PAM_ERROR_MSG => conv.problem(&text),
            PAM_TEXT_INFO => conv.info(&text),
            _ => {
                // SAFETY: as above.
                unsafe { free_replies(replies, n) };
                return PAM_CONV_ERR;
            }
        }
    }
    // SAFETY: PAM takes ownership of the array and its strings.
    unsafe { *resp = replies };
    PAM_SUCCESS
}

/// Wipe and free a reply array that will not reach PAM.
unsafe fn free_replies(replies: *mut PamResponse, n: usize) {
    for i in 0..n {
        // SAFETY: per the caller, `replies` holds `n` entries, each NULL or malloc'd and
        // NUL-terminated.
        unsafe {
            let resp = (*replies.add(i)).resp;
            if !resp.is_null() {
                let len = libc::strlen(resp);
                wipe(std::slice::from_raw_parts_mut(resp.cast::<u8>(), len));
                libc::free(resp.cast());
            }
        }
    }
    // SAFETY: calloc'd by `relay`.
    unsafe { libc::free(replies.cast()) };
}

/// One PAM transaction. `pam_end` runs on drop with the last status.
pub struct Pam {
    handle: *mut PamHandle,
    // `pam_start` keeps pointers to both for the transaction's lifetime.
    _conv: Box<PamConv>,
    _relay: Option<Box<Box<dyn Conversation>>>,
    status: c_int,
}

impl Pam {
    /// A transaction whose modules may ask nothing.
    pub fn start(service: &str, user: &str) -> anyhow::Result<Self> {
        Self::open(service, user, None)
    }

    /// A transaction whose questions and messages go to `conversation`.
    pub fn start_with(
        service: &str,
        user: &str,
        conversation: Box<dyn Conversation>,
    ) -> anyhow::Result<Self> {
        Self::open(service, user, Some(Box::new(conversation)))
    }

    fn open(
        service: &str,
        user: &str,
        mut relay_to: Option<Box<Box<dyn Conversation>>>,
    ) -> anyhow::Result<Self> {
        let service = CString::new(service)?;
        let user = CString::new(user)?;
        let conv = Box::new(match relay_to.as_mut() {
            Some(conversation) => PamConv {
                conv: relay,
                appdata: (&mut **conversation as *mut Box<dyn Conversation>).cast(),
            },
            None => PamConv {
                conv: refuse_all,
                appdata: ptr::null_mut(),
            },
        });
        let mut handle = ptr::null_mut();
        // SAFETY: valid C strings; `conv` and what it points at outlive the handle (both live in
        // `Self`, boxed, so they do not move).
        let ret = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &*conv, &mut handle) };
        if ret != PAM_SUCCESS || handle.is_null() {
            bail!("pam_start: error {ret}");
        }
        Ok(Self {
            handle,
            _conv: conv,
            _relay: relay_to,
            status: PAM_SUCCESS,
        })
    }

    /// `pam_authenticate`, returning PAM's code: the caller tells the user which failure it was.
    pub fn authenticate_code(&mut self) -> c_int {
        // SAFETY: live handle.
        self.status = unsafe { pam_authenticate(self.handle, 0) };
        self.status
    }

    /// `pam_acct_mgmt`, returning PAM's code.
    pub fn acct_mgmt_code(&mut self) -> c_int {
        // SAFETY: live handle.
        self.status = unsafe { pam_acct_mgmt(self.handle, 0) };
        self.status
    }

    pub fn chauthtok_expired(&mut self) -> anyhow::Result<()> {
        // SAFETY: live handle.
        let ret = unsafe { pam_chauthtok(self.handle, PAM_CHANGE_EXPIRED_AUTHTOK) };
        self.check("pam_chauthtok", ret)
    }

    pub fn strerror(&self, code: c_int) -> String {
        // SAFETY: the handle is live; libpam returns a static string.
        unsafe { CStr::from_ptr(pam_strerror(self.handle, code)) }
            .to_string_lossy()
            .into_owned()
    }

    fn check(&mut self, what: &str, ret: c_int) -> anyhow::Result<()> {
        self.status = ret;
        if ret == PAM_SUCCESS {
            return Ok(());
        }
        // SAFETY: the handle is live; libpam returns a static string.
        let msg = unsafe { CStr::from_ptr(pam_strerror(self.handle, ret)) };
        bail!("{what}: {}", msg.to_string_lossy())
    }

    pub fn authenticate(&mut self) -> anyhow::Result<()> {
        // SAFETY: live handle.
        let ret = unsafe { pam_authenticate(self.handle, 0) };
        self.check("pam_authenticate", ret)
    }

    pub fn acct_mgmt(&mut self) -> anyhow::Result<()> {
        // SAFETY: live handle.
        let ret = unsafe { pam_acct_mgmt(self.handle, 0) };
        self.check("pam_acct_mgmt", ret)
    }

    pub fn setcred(&mut self, flags: c_int) -> anyhow::Result<()> {
        // SAFETY: live handle.
        let ret = unsafe { pam_setcred(self.handle, flags) };
        self.check("pam_setcred", ret)
    }

    pub fn open_session(&mut self) -> anyhow::Result<()> {
        // SAFETY: live handle.
        let ret = unsafe { pam_open_session(self.handle, 0) };
        self.check("pam_open_session", ret)
    }

    pub fn close_session(&mut self) -> anyhow::Result<()> {
        // SAFETY: live handle.
        let ret = unsafe { pam_close_session(self.handle, 0) };
        self.check("pam_close_session", ret)
    }

    pub fn putenv(&mut self, name: &str, value: &str) -> anyhow::Result<()> {
        let pair = CString::new(format!("{name}={value}"))?;
        // SAFETY: live handle; libpam copies the string.
        let ret = unsafe { pam_putenv(self.handle, pair.as_ptr()) };
        self.check("pam_putenv", ret)
    }

    /// `PAM_TTY`: the terminal the session is on, which `pam_systemd` reads its VT from.
    pub fn set_tty(&mut self, tty: &str) -> anyhow::Result<()> {
        const PAM_TTY: c_int = 3;
        let tty = CString::new(tty)?;
        // SAFETY: live handle; libpam copies the string.
        let ret = unsafe { pam_set_item(self.handle, PAM_TTY, tty.as_ptr().cast()) };
        self.check("pam_set_item(PAM_TTY)", ret)
    }

    pub fn getenv(&self, name: &str) -> Option<String> {
        let name = CString::new(name).ok()?;
        // SAFETY: live handle; the result is owned by the handle and copied before return.
        let value = unsafe { pam_getenv(self.handle, name.as_ptr()) };
        (!value.is_null()).then(|| {
            unsafe { CStr::from_ptr(value) }
                .to_string_lossy()
                .into_owned()
        })
    }

    /// The PAM environment as `NAME=value` strings.
    pub fn envlist(&self) -> anyhow::Result<Vec<CString>> {
        // SAFETY: live handle. The array and each string are malloc'd copies the caller frees.
        let list = unsafe { pam_getenvlist(self.handle) };
        if list.is_null() {
            bail!("pam_getenvlist failed");
        }
        let mut out = Vec::new();
        let mut at = list;
        // SAFETY: a NULL-terminated array of malloc'd strings, freed here once copied.
        unsafe {
            while !(*at).is_null() {
                out.push(CStr::from_ptr(*at).to_owned());
                libc::free((*at).cast());
                at = at.add(1);
            }
            libc::free(list.cast());
        }
        Ok(out)
    }

    /// End the transaction without undoing what it set up: for the session's child, whose
    /// session the parent still owns (`pam_end(3)`, `PAM_DATA_SILENT`).
    pub fn end_silently(mut self) {
        // SAFETY: live handle, ended exactly once (drop sees it null).
        unsafe { pam_end(self.handle, self.status | PAM_DATA_SILENT) };
        self.handle = ptr::null_mut();
    }
}

impl Drop for Pam {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: live handle, ended exactly once.
            unsafe { pam_end(self.handle, self.status) };
        }
    }
}
