// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::time::Duration;

use calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use synoik_config::Config;

use crate::backend::BackendMode;
use crate::synoik::State;

pub struct Server {
    pub event_loop: EventLoop<'static, State>,
    pub state: State,
    /// The receiving half of the shield's gdm request channel, kept alive for the fixture's
    /// lifetime. See [`Server::new`] — dropping it would close the channel and turn every lock
    /// into "nobody to ask".
    ///
    /// Readable, because it is the only place a test can see what the shield actually asked gdm
    /// for — `StartFingerprint` has no other observable effect in-process.
    pub gdm_requests: async_channel::Receiver<crate::dbus::gdm::VerifierRequest>,
}

impl Server {
    pub fn new(config: Config) -> Self {
        let event_loop = EventLoop::try_new().unwrap();
        let handle = event_loop.handle();
        let display = Display::new().unwrap();
        let mut state = State::new(
            config,
            handle.clone(),
            event_loop.get_signal(),
            display,
            BackendMode::HeadlessTest,
            crate::synoik::WaylandSocket::None,
            false,
        )
        .unwrap();

        // Stand in for the gdm client the session instance starts. Tests simulate the answers by
        // driving `State::on_verifier_event`, and without a channel to send `Begin` down the shield
        // would refuse to lock at all — correctly, since a lock nobody can answer is a lockout, but
        // it would leave the whole locked-shield corpus untestable.
        let (to_gdm, from_niri) = async_channel::unbounded();
        {
            state.synoik.gdm_requests = Some(to_gdm);
        }

        // Recording paths reach the real filesystem: a test that drives the capture button in
        // cast mode goes through `default_recording_path`, which resolves the XDG Videos
        // directory. That left an empty recording in the developer's own `~/Videos/Screencasts`
        // on every suite run. Point it somewhere disposable instead — per process, so parallel
        // test binaries do not share it.
        state.synoik.recordings_base = Some(recordings_base());

        Self {
            event_loop,
            state,
            gdm_requests: from_niri,
        }
    }

    pub fn dispatch(&mut self) {
        self.event_loop
            .dispatch(Duration::ZERO, &mut self.state)
            .unwrap();
        self.state.refresh_and_flush_clients();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Whatever a recording test wrote there is scratch; the path is this process's own.
        std::fs::remove_dir_all(recordings_base()).ok();
    }
}

/// The scratch directory recordings land in under test. Per process, so two test binaries running
/// at once cannot delete each other's files on drop.
fn recordings_base() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("synoik-test-recordings-{}", std::process::id()))
}
