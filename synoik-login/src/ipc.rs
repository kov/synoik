// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The daemon ↔ worker wire: a stream socket on the worker's fd 3, framed as a tag byte, a
//! little-endian `u32` length and that many bytes.
//!
//! Answers carry secrets, so their buffers are [`Secret`]s: wiped when dropped.

use std::io::{self, Read, Write};

/// One frame. Text is UTF-8; an answer is bytes, as PAM takes it.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    // Worker → daemon.
    /// A visible question (`PAM_PROMPT_ECHO_ON`).
    InfoQuery(String),
    /// A hidden one (`PAM_PROMPT_ECHO_OFF`).
    SecretInfoQuery(String),
    Info(String),
    Problem(String),
    /// Authenticated: `VerificationComplete`.
    Complete,
    /// Refused, with what to tell the user.
    Failed(String),
    /// The service cannot run at all (no reader, a missing module): `ServiceUnavailable`.
    Unavailable(String),
    /// A session opened, with its logind id.
    Opened(String),
    // Daemon → worker.
    Answer(Secret),
}

/// Bytes that are wiped before their memory is freed.
#[derive(PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Secret(..)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

/// Zero `bytes` in a way the optimiser may not remove.
pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: a valid, aligned &mut u8.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// Longest frame accepted: PAM's own limit on a message, with room to spare.
const MAX_LEN: usize = 64 * 1024;

impl Frame {
    fn tag(&self) -> u8 {
        match self {
            Frame::InfoQuery(_) => 1,
            Frame::SecretInfoQuery(_) => 2,
            Frame::Info(_) => 3,
            Frame::Problem(_) => 4,
            Frame::Complete => 5,
            Frame::Failed(_) => 6,
            Frame::Unavailable(_) => 7,
            Frame::Opened(_) => 8,
            Frame::Answer(_) => 9,
        }
    }

    fn payload(&self) -> &[u8] {
        match self {
            Frame::InfoQuery(s)
            | Frame::SecretInfoQuery(s)
            | Frame::Info(s)
            | Frame::Problem(s)
            | Frame::Failed(s)
            | Frame::Unavailable(s)
            | Frame::Opened(s) => s.as_bytes(),
            Frame::Complete => &[],
            Frame::Answer(secret) => secret.as_bytes(),
        }
    }

    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let payload = self.payload();
        let mut header = [0u8; 5];
        header[0] = self.tag();
        header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        // One write per frame, so a frame is never interleaved with another writer's.
        let mut buf = Vec::with_capacity(5 + payload.len());
        buf.extend_from_slice(&header);
        buf.extend_from_slice(payload);
        let result = out.write_all(&buf);
        wipe(&mut buf);
        result
    }

    /// The next frame, or `None` at a clean end of stream.
    pub fn read_from(input: &mut impl Read) -> io::Result<Option<Frame>> {
        let mut header = [0u8; 5];
        match input.read_exact(&mut header) {
            Ok(()) => (),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        }
        let len = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
        if len > MAX_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too long"));
        }
        let mut payload = vec![0u8; len];
        input.read_exact(&mut payload)?;
        let text = |payload: Vec<u8>| {
            String::from_utf8(payload)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame is not UTF-8"))
        };
        Ok(Some(match header[0] {
            1 => Frame::InfoQuery(text(payload)?),
            2 => Frame::SecretInfoQuery(text(payload)?),
            3 => Frame::Info(text(payload)?),
            4 => Frame::Problem(text(payload)?),
            5 => Frame::Complete,
            6 => Frame::Failed(text(payload)?),
            7 => Frame::Unavailable(text(payload)?),
            8 => Frame::Opened(text(payload)?),
            9 => Frame::Answer(Secret::new(payload)),
            tag => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame {tag}"),
                ))
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = vec![
            Frame::SecretInfoQuery("Password:".into()),
            Frame::Info("".into()),
            Frame::Complete,
            Frame::Answer(Secret::new(b"hunter2".to_vec())),
            Frame::Opened("c1".into()),
        ];
        let mut wire = Vec::new();
        for frame in &frames {
            frame.write_to(&mut wire).unwrap();
        }
        let mut input = &wire[..];
        for frame in frames {
            assert_eq!(Frame::read_from(&mut input).unwrap(), Some(frame));
        }
        assert_eq!(Frame::read_from(&mut input).unwrap(), None);
    }

    #[test]
    fn a_secret_never_prints() {
        let frame = Frame::Answer(Secret::new(b"hunter2".to_vec()));
        assert!(!format!("{frame:?}").contains("hunter2"));
    }

    #[test]
    fn an_oversized_frame_is_refused() {
        let mut wire = vec![1];
        wire.extend_from_slice(&(u32::MAX).to_le_bytes());
        assert!(Frame::read_from(&mut &wire[..]).is_err());
    }
}
