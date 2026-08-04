//! We set a signal handler with `calloop::signals::Signals::new`.
//! This does two things:
//! 1. It blocks the thread from receiving these signals normally (pthread_sigmask)
//! 2. It creates a signalfd to read them in the event loop.
//!
//! When spawning children, calloop already deals with the signalfd.
//! `Signals::new` creates it with CLOEXEC, so it will not be inherited by children.
//!
//! But, the sigmask is always inherited, so we want to clear it before spawning children.
//! That way, we don't affect their normal signal handling.
//!
//! In particular, if a child doesn't care about signals, we must not block it from receiving them.
//!
//! This module provides functions to clear the sigmask. Call them before spawning children.
//!
//! Technically, a "more correct" solution would be to remember the original sigmask and restore it
//! after the child exits, but that's painful *and* likely to cause issues, because the user almost
//! never intended to spawn synoik with a nonempty sigmask. It indicates a bug in whoever spawned
//! us, so we may as well clean up after them (which is easier than not doing so).

pub use platform::*;

#[cfg(not(target_os = "linux"))]
mod platform {
    use std::io;

    // FIXME: implement for FreeBSD. But probably, that should be done in calloop::signals.
    pub fn listen(_handle: &calloop::LoopHandle<crate::synoik::State>) {}

    // These two actually build as-is on FreeBSD, but without our own signal handling in listen(),
    // they do more harm than good (they block termination signals without actually installing a
    // termination handler).
    pub fn block_early() -> io::Result<()> {
        Ok(())
    }
    pub fn unblock_all() -> io::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{io, mem};

    pub fn listen(handle: &calloop::LoopHandle<crate::synoik::State>) {
        use calloop::signals::{Signal, Signals};

        handle
            .insert_source(
                Signals::new(&[Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP]).unwrap(),
                |event, _, state| {
                    info!("quitting due to receiving signal {:?}", event.signal());
                    // Quit outright, as mutter's SIGTERM handler does: `meta_context_terminate`
                    // is a bare `g_main_loop_quit` (`meta-context.c:519-530`). Waiting for the
                    // clients instead is what `docs/fork/session-end.md` §6 records as the wrong
                    // shape — systemd is already stopping them, and it is ordered to stop us last.
                    state.synoik.stop_signal.stop();
                },
            )
            .unwrap();

        // Its own source, because this one must not terminate the session:
        // SIGUSR1 dumps the frame log's in-memory ring (`SYNOIK_FRAME_LOG=ring`).
        handle
            .insert_source(
                Signals::new(&[Signal::SIGUSR1]).unwrap(),
                |_event, _, state| match state.synoik.frame_log.dump() {
                    Ok((path, 0)) => {
                        info!("frame-log ring is empty (is SYNOIK_FRAME_LOG=ring set?); {path:?}")
                    }
                    Ok((path, n)) => info!("dumped {n} frame-log entries to {}", path.display()),
                    Err(err) => warn!("error dumping the frame log: {err}"),
                },
            )
            .unwrap();
    }

    // We block the signals early, so that they apply to all threads.
    // They are then blocked *again* by the `Signals` source. That's fine.
    pub fn block_early() -> io::Result<()> {
        set_sigmask(&preferred_sigset()?)
    }

    pub fn unblock_all() -> io::Result<()> {
        set_sigmask(&empty_sigset()?)
    }

    fn empty_sigset() -> io::Result<libc::sigset_t> {
        let mut sigset = mem::MaybeUninit::uninit();
        if unsafe { libc::sigemptyset(sigset.as_mut_ptr()) } == 0 {
            Ok(unsafe { sigset.assume_init() })
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn preferred_sigset() -> io::Result<libc::sigset_t> {
        let mut set = empty_sigset()?;
        unsafe {
            add_signal(&mut set, libc::SIGINT)?;
            add_signal(&mut set, libc::SIGTERM)?;
            add_signal(&mut set, libc::SIGHUP)?;
            // Blocked from the start too: until the `Signals` source below is
            // installed, the default action for SIGUSR1 is to kill the process —
            // so an early dump request would take the session down.
            add_signal(&mut set, libc::SIGUSR1)?;
        }
        Ok(set)
    }

    // SAFETY: `signum` must be a valid signal number.
    unsafe fn add_signal(set: &mut libc::sigset_t, signum: libc::c_int) -> io::Result<()> {
        if unsafe { libc::sigaddset(set, signum) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn set_sigmask(set: &libc::sigset_t) -> io::Result<()> {
        let oldset = std::ptr::null_mut(); // ignore old mask
        if unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, set, oldset) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
