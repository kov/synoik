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

pub const PAM_SUCCESS: c_int = 0;
const PAM_CONV_ERR: c_int = 19;

pub const PAM_ESTABLISH_CRED: c_int = 0x0002;
pub const PAM_DELETE_CRED: c_int = 0x0004;
pub const PAM_DATA_SILENT: c_int = 0x4000_0000;

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
    fn pam_setcred(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_open_session(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_close_session(handle: *mut PamHandle, flags: c_int) -> c_int;
    fn pam_putenv(handle: *mut PamHandle, name_value: *const c_char) -> c_int;
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

/// One PAM transaction. `pam_end` runs on drop with the last status.
pub struct Pam {
    handle: *mut PamHandle,
    // `pam_start` keeps a pointer to it for the transaction's lifetime.
    _conv: Box<PamConv>,
    status: c_int,
}

impl Pam {
    pub fn start(service: &str, user: &str) -> anyhow::Result<Self> {
        let service = CString::new(service)?;
        let user = CString::new(user)?;
        let conv = Box::new(PamConv {
            conv: refuse_all,
            appdata: ptr::null_mut(),
        });
        let mut handle = ptr::null_mut();
        // SAFETY: valid C strings; `conv` outlives the handle (it lives in `Self`).
        let ret = unsafe { pam_start(service.as_ptr(), user.as_ptr(), &*conv, &mut handle) };
        if ret != PAM_SUCCESS || handle.is_null() {
            bail!("pam_start: error {ret}");
        }
        Ok(Self {
            handle,
            _conv: conv,
            status: PAM_SUCCESS,
        })
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
