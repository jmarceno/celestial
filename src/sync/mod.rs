//! Background sync. The Qt thread only sends commands and reads snapshots.

mod engine;
pub mod model;

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
    ChooseConflict {
        remote: String,
        local: String,
        remote_path: String,
        id: u64,
        keep_local: bool,
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
    Suggestions(String),
}

#[derive(Clone)]
pub struct Shared {
    pub bus: model::SnapshotBus,
    pub quit: Arc<AtomicBool>,
    pub stopped: Arc<AtomicBool>,
    pub cancel_auth: Arc<AtomicBool>,
    pub notices: crossbeam_channel::Sender<Notice>,
    pub tray: Arc<dyn Fn(String, String) + Send + Sync>,
}

impl Shared {
    pub fn publish(&self, model: &mut model::Model) {
        (self.tray)(model.tray_message.clone(), model.tray_phase.clone());
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
