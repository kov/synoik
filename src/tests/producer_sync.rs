// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Producer-side client-buffer synchronization.
//!
//! Producer readiness (the client's buffer is done being written before we sample it) is gated
//! renderer-agnostically at commit time by the dmabuf pre-commit hook installed on every surface
//! — it blocks the commit on the buffer's implicit fence, or, for `linux-drm-syncobj-v1` clients,
//! on the explicit acquire timeline point. See `docs/fork/explicit-sync.md`.
//!
//! The explicit-sync *negotiation* can't be exercised headlessly: the protocol global is only
//! advertised on the tty backend (it needs a real DRM device for `DrmSyncobjState`), so the
//! headless `Fixture` never offers it and a test client can't set an acquire point. Nor can the
//! *implicit* dmabuf blocker fire here — the test client only attaches shm buffers, which carry
//! no producing fence. Both blocker paths are covered by anvil-parity in the pre-commit hooks
//! plus live validation on the gsrs seat. What we pin here is the one thing headless can observe:
//! that the producer-sync pre-commit hook is actually installed on every surface.

use super::*;

/// A new surface must get the default dmabuf pre-commit hook (the entry point for both the
/// implicit-fence and explicit-sync acquire blockers), and installation must not become gated on
/// the `debug.disable_transactions` flag. This only checks *installation* — the blocker logic
/// itself is validated by anvil-parity + live validation (see the module comment and
/// `docs/fork/explicit-sync.md`).
#[test]
fn dmabuf_pre_commit_hook_installed_on_every_surface() {
    for disable_transactions in [false, true] {
        let mut config = Config::default();
        config.debug.disable_transactions = disable_transactions;

        let mut f = Fixture::with_config(config);
        f.add_output(1, (1920, 1080));
        let id = f.add_client();

        let before = f.synoik().dmabuf_pre_commit_hook.len();
        f.client(id).create_window();
        f.roundtrip(id);
        let after = f.synoik().dmabuf_pre_commit_hook.len();

        assert_eq!(
            after - before,
            1,
            "a new surface must get the default dmabuf pre-commit hook \
             (disable_transactions={disable_transactions})",
        );
    }
}
