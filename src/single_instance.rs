//! One running instance per user session.
//!
//! The first (primary) instance binds `app.sock` under the runtime dir and
//! listens. A later launch connects, writes `activate\n` and exits; the
//! primary turns that into [`Activation`] so the UI can show its window.
//! A stale socket file left by a crash is detected (connect fails) and
//! replaced.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ACTIVATE_MSG: &[u8] = b"activate\n";

/// Delivered to the primary when another launch happens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activation;

pub enum AcquireOutcome {
    /// Hold the guard for the app lifetime.
    Primary(Guard),
    /// Another instance is running and was told to activate; exit.
    Secondary,
}

/// Owns the socket path; dropping it removes the socket file.
pub struct Guard {
    socket_path: PathBuf,
    rx: crossbeam_channel::Receiver<Activation>,
}

impl Guard {
    pub fn receiver(&self) -> crossbeam_channel::Receiver<Activation> {
        self.rx.clone()
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

pub fn socket_path() -> PathBuf {
    crate::util::runtime_dir().join("app.sock")
}

pub fn acquire() -> Result<AcquireOutcome, String> {
    acquire_on(&socket_path())
}

pub fn acquire_on(path: &Path) -> Result<AcquireOutcome, String> {
    let parent = path.parent().ok_or("socket path has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("socket dir: {e}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }

    if notify(path) {
        return Ok(AcquireOutcome::Secondary);
    }
    // Only reached when nothing accepts connections: the file is stale.
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        // Lost a race with another starting primary.
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && notify(path) => {
            return Ok(AcquireOutcome::Secondary);
        }
        Err(e) => return Err(format!("bind {}: {e}", path.display())),
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || accept_loop(listener, tx))
        .map_err(|e| format!("spawn listener: {e}"))?;
    Ok(AcquireOutcome::Primary(Guard {
        socket_path: path.to_path_buf(),
        rx,
    }))
}

/// True when a live primary accepted the activation message.
fn notify(path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    stream.write_all(ACTIVATE_MSG).is_ok()
}

fn accept_loop(listener: UnixListener, tx: crossbeam_channel::Sender<Activation>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 64];
        // Zero bytes = bare liveness probe; do not wake the UI for it.
        if matches!(stream.read(&mut buf), Ok(n) if n > 0) && tx.send(Activation).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recv(rx: &crossbeam_channel::Receiver<Activation>) -> Option<Activation> {
        rx.recv_timeout(Duration::from_secs(3)).ok()
    }

    #[test]
    fn primary_then_secondary_then_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("rt").join("app.sock");
        let AcquireOutcome::Primary(guard) = acquire_on(&sock).unwrap() else {
            panic!("expected primary");
        };
        let rx = guard.receiver();
        assert!(matches!(
            acquire_on(&sock).unwrap(),
            AcquireOutcome::Secondary
        ));
        assert_eq!(recv(&rx), Some(Activation));
        drop(guard);
        assert!(!sock.exists());
    }

    #[test]
    fn stale_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("app.sock");
        std::fs::write(&sock, b"stale").unwrap();
        assert!(matches!(
            acquire_on(&sock).unwrap(),
            AcquireOutcome::Primary(_)
        ));
    }
}
