//! Background sync. The Qt thread only sends commands and reads snapshots.

mod capabilities;
mod engine;
mod exclusions;
mod failpoint;
mod localfs;
pub mod measure;
pub mod model;
mod observe;
mod pass;
pub mod progress;
mod reconcile;
mod recovery;
mod store;
#[cfg(test)]
mod testing;
mod watch;

pub use engine::spawn;

use crate::login::LoginRequest;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub enum Command {
    SubmitLogin(LoginRequest),
    DeleteRemote(String),
    StopDir {
        remote: String,
        local: String,
        remote_path: String,
    },
    AddDir {
        remote: String,
        local: String,
        remote_path: String,
    },
    SaveExclusions {
        local: String,
        text: String,
    },
    ActivateError {
        remote: String,
        local: String,
        remote_path: String,
        id: u64,
    },
    /// Delete an item on both sides after an edit/delete outcome. `token`
    /// is what was shown; the action is refused if either side changed.
    ConfirmDeletion {
        remote: String,
        local: String,
        remote_path: String,
        id: u64,
        token: String,
    },
    /// Put a preserved version back.
    Restore {
        remote: String,
        local: String,
        remote_path: String,
        recovery_id: i64,
    },
    DismissError {
        remote: String,
        local: String,
        remote_path: String,
        id: u64,
    },
    ListDirs {
        remote: String,
        path: String,
    },
    Quit,
}

#[derive(Debug, Clone)]
pub enum Notice {
    Dialog {
        kind: String,
        title: String,
        body: String,
    },
    /// Subfolders of one remote folder, for the remote folder browser.
    Folders(String),
    /// A folder estimate, partial or final (see [`measure`]).
    Estimate(String),
}

#[derive(Clone)]
pub struct Shared {
    pub bus: model::SnapshotBus,
    pub quit: Arc<AtomicBool>,
    pub stopped: Arc<AtomicBool>,
    pub cancel_auth: Arc<AtomicBool>,
    pub notices: crossbeam_channel::Sender<Notice>,
    pub tray: Arc<dyn Fn(String, String) + Send + Sync>,
    /// Live transfer progress, read by the window and the tray.
    pub progress: progress::ProgressBus,
    /// The tray's progress line; `None` hides it.
    pub tray_progress: Arc<dyn Fn(Option<String>) + Send + Sync>,
}

impl Shared {
    /// Publish the model. Identical snapshots and tray states are not sent
    /// again, so idle loops cost the window and the tray nothing.
    pub fn publish(&self, model: &mut model::Model) {
        if self
            .bus
            .tray_changed(&model.tray_message, &model.tray_phase)
        {
            (self.tray)(model.tray_message.clone(), model.tray_phase.clone());
        }
        self.bus.publish(model);
    }

    pub fn set_tray(&self, model: &mut model::Model, message: String, phase: &str) {
        model.tray_message = message;
        model.tray_phase = phase.to_owned();
        self.publish(model);
    }

    pub fn dialog(&self, kind: &str, title: impl Into<String>, body: impl Into<String>) {
        let _ = self.notices.send(Notice::Dialog {
            kind: kind.to_owned(),
            title: title.into(),
            body: body.into(),
        });
    }

    pub fn quit_requested(&self) -> bool {
        self.quit.load(Ordering::Relaxed)
    }
}
