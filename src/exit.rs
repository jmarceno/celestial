//! Leaving the app. An order to quit, from the person or from the session
//! (shutdown, logout), is obeyed at once, whatever is running: the process
//! ends without waiting for transfers, the sync worker, Qt or the Go runtime
//! of librclone.
//!
//! This loses nothing. Every sync operation is journaled and downloads are
//! staged inside the pair's internal folder, so an interruption at any point
//! is replayed or undone on the next start (see `sync::recovery`), and
//! SQLite keeps every committed transaction. A stale single-instance socket
//! is replaced on the next start.

use nix::libc;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

/// End the process now with `code`.
pub fn now(code: i32) -> ! {
    // SAFETY: `_exit` only ends the process. It skips the exit handlers on
    // purpose: they could wait on threads that are busy (Qt, Go).
    unsafe { libc::_exit(code) }
}

/// End the process at once on SIGTERM, SIGINT and SIGHUP, so shutdown and
/// logout are never held up. rclone takes SIGINT and SIGTERM over the first
/// time it copies a file and then runs its own cleanup before exiting; call
/// this again after [`crate::rclone::claim_exit_signals`] to take them back.
pub fn on_signals() {
    let action = SigAction::new(
        SigHandler::Handler(terminate),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        // SAFETY: the handler only calls `_exit`, which is async-signal-safe.
        if let Err(err) = unsafe { sigaction(signal, &action) } {
            tracing::warn!("could not handle {signal}: {err}");
        }
    }
}

extern "C" fn terminate(signal: libc::c_int) {
    // SAFETY: async-signal-safe, see `now`.
    unsafe { libc::_exit(128 + signal) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handler(signal: Signal) -> SigHandler {
        let probe = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        // SAFETY: the previous action is put back at once.
        unsafe {
            let old = sigaction(signal, &probe).expect("read handler");
            sigaction(signal, &old).expect("restore handler");
            old.handler()
        }
    }

    #[test]
    fn signals_stay_ours_after_rclone_copies_files() {
        let _guard = crate::sync::testing::rclone_guard();
        crate::rclone::claim_exit_signals().expect("throwaway copy");
        // rclone's Go runtime now handles SIGTERM itself.
        assert_ne!(handler(Signal::SIGTERM), SigHandler::SigDfl);
        on_signals();
        // A later copy, like a real transfer, does not take them back.
        crate::rclone::claim_exit_signals().expect("second copy");
        for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            assert_eq!(handler(signal), SigHandler::Handler(terminate), "{signal}");
        }
        let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            // SAFETY: back to the default disposition for the test runner.
            unsafe { sigaction(signal, &default) }.expect("reset handler");
        }
    }
}
