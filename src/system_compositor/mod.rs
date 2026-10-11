// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The system compositor and the wire it shares with nested sessions
//! (`docs/fork/system-compositor.md`). The session side is `backend::nested`.

pub mod outer;
pub mod protocol;
