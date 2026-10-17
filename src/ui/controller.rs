//! Rust <-> QML bridge.
//!
//! Invokables and `tick()` run on the Qt thread. They only enqueue work.
//! The sync worker owns rclone and the database.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use cxx_qt::CxxQtType;
use cxx_qt_lib::QString;

use crate::login::LoginRequest;
use crate::single_instance::Activation;
use crate::sync::{self, Command, Notice, Shared};
use crate::ui::tray::{TrayEvent, TrayHandle};

pub struct UiBootstrap {
    pub activations: Option<crossbeam_channel::Receiver<Activation>>,
    pub tray: Option<Arc<TrayHandle>>,
    pub tray_rx: Option<crossbeam_channel::Receiver<TrayEvent>>,
    pub background: bool,
}

static BOOTSTRAP: OnceLock<Mutex<Option<UiBootstrap>>> = OnceLock::new();

pub fn install_bootstrap(boot: UiBootstrap) {
    let cell = BOOTSTRAP.get_or_init(|| Mutex::new(None));
    *cell.lock().expect("bootstrap lock") = Some(boot);
}

fn take_bootstrap() -> Option<UiBootstrap> {
    BOOTSTRAP.get().and_then(|cell| cell.lock().ok()?.take())
}

pub(crate) fn is_bootstrap_pending() -> bool {
    BOOTSTRAP
        .get()
        .is_none_or(|cell| cell.lock().map(|guard| guard.is_some()).unwrap_or(false))
}

#[cxx_qt::bridge]
mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    unsafe extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, snapshot_json)]
        #[qproperty(QString, folders_json)]
        #[qproperty(QString, progress_json)]
        #[qproperty(QString, estimate_json)]
        #[qproperty(bool, window_visible)]
        type AppController = super::AppControllerRust;

        #[qsignal]
        fn activated(self: Pin<&mut Self>);

        /// The tray icon was clicked: show the status window near `x`, `y`.
        #[qsignal]
        #[cxx_name = "trayActivated"]
        fn tray_activated(self: Pin<&mut Self>, x: i32, y: i32);

        #[qsignal]
        #[cxx_name = "notice"]
        fn notice(self: Pin<&mut Self>, kind: QString, title: QString, body: QString);

        #[qinvokable]
        fn bootstrap(self: Pin<&mut Self>);

        #[qinvokable]
        fn tick(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "hideWindow"]
        fn hide_window(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "showWindow"]
        fn show_window(self: Pin<&mut Self>);

        #[qinvokable]
        fn quit(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "submitLogin"]
        fn submit_login(self: Pin<&mut Self>, payload: &QString);

        #[qinvokable]
        #[cxx_name = "cancelAuth"]
        fn cancel_auth(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "deleteRemote"]
        fn delete_remote(self: Pin<&mut Self>, name: &QString);

        #[qinvokable]
        #[cxx_name = "stopDir"]
        fn stop_dir(self: Pin<&mut Self>, remote: &QString, local: &QString, remote_path: &QString);

        #[qinvokable]
        #[cxx_name = "addDir"]
        fn add_dir(self: Pin<&mut Self>, remote: &QString, local: &QString, remote_path: &QString);

        /// Start a folder pair that was added paused.
        #[qinvokable]
        #[cxx_name = "resumeDir"]
        fn resume_dir(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
        );

        #[qinvokable]
        #[cxx_name = "saveExclusions"]
        fn save_exclusions(self: Pin<&mut Self>, local: &QString, text: &QString);

        #[qinvokable]
        #[cxx_name = "activateError"]
        fn activate_error(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
            id: i64,
        );

        #[qinvokable]
        #[cxx_name = "confirmDeletion"]
        fn confirm_deletion(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
            id: i64,
            token: &QString,
        );

        #[qinvokable]
        #[cxx_name = "restoreVersion"]
        fn restore_version(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
            id: i64,
        );

        #[qinvokable]
        #[cxx_name = "deleteVersion"]
        fn delete_version(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
            id: i64,
        );

        #[qinvokable]
        #[cxx_name = "dismissError"]
        fn dismiss_error(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
            id: i64,
        );

        /// Estimate a folder pair before it is added; results arrive in
        /// `estimate_json`.
        #[qinvokable]
        #[cxx_name = "estimateDir"]
        fn estimate_dir(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
        );

        #[qinvokable]
        #[cxx_name = "cancelEstimate"]
        fn cancel_estimate(self: Pin<&mut Self>);

        #[qinvokable]
        #[cxx_name = "requestStats"]
        fn request_stats(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
        );

        #[qinvokable]
        #[cxx_name = "listDirs"]
        fn list_dirs(self: Pin<&mut Self>, remote: &QString, path: &QString);

        /// Open the system folder chooser. The choice arrives as a
        /// `local-folder` notice, or `local-folder-fallback` when the
        /// desktop has no portal chooser.
        #[qinvokable]
        #[cxx_name = "pickLocalFolder"]
        fn pick_local_folder(self: Pin<&mut Self>, start: &QString);
    }
}

struct Runtime {
    shared: Shared,
    commands: crossbeam_channel::Sender<Command>,
    notices: crossbeam_channel::Receiver<Notice>,
    tray_rx: Option<crossbeam_channel::Receiver<TrayEvent>>,
    activations: Option<crossbeam_channel::Receiver<Activation>>,
    seen_gen: u64,
    seen_progress: u64,
    background: bool,
    saw_snapshot: bool,
}

pub struct AppControllerRust {
    snapshot_json: QString,
    folders_json: QString,
    progress_json: QString,
    estimate_json: QString,
    window_visible: bool,
    runtime: Option<Runtime>,
}

impl Default for AppControllerRust {
    fn default() -> Self {
        Self {
            snapshot_json: QString::from("{}"),
            folders_json: QString::from("{}"),
            progress_json: QString::from("{\"pairs\":[],\"total\":null}"),
            estimate_json: QString::from("{}"),
            window_visible: false,
            runtime: None,
        }
    }
}

impl qobject::AppController {
    fn bootstrap(mut self: Pin<&mut Self>) {
        let Some(boot) = take_bootstrap() else {
            tracing::warn!("bootstrap() called twice or without a payload");
            return;
        };
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (notice_tx, notice_rx) = crossbeam_channel::unbounded();
        let cancel_auth = Arc::new(AtomicBool::new(false));
        let tray = boot.tray.clone();
        let progress_tray = boot.tray.clone();
        let shared = Shared {
            bus: crate::sync::model::SnapshotBus::new(),
            cancel_auth,
            notices: notice_tx,
            tray: Arc::new(move |message, phase| {
                if let Some(tray) = tray.as_ref() {
                    tray.update(message, phase);
                }
            }),
            progress: Default::default(),
            tray_progress: Arc::new(move |progress| {
                if let Some(tray) = progress_tray.as_ref() {
                    tray.update_progress(progress);
                }
            }),
        };
        sync::spawn(shared.clone(), cmd_rx);
        self.as_mut().rust_mut().runtime = Some(Runtime {
            shared,
            commands: cmd_tx,
            notices: notice_rx,
            tray_rx: boot.tray_rx,
            activations: boot.activations,
            seen_gen: 0,
            seen_progress: 0,
            background: boot.background,
            saw_snapshot: false,
        });
        self.as_mut().set_window_visible(!boot.background);
        tracing::info!("QML bootstrap complete");
    }

    fn tick(mut self: Pin<&mut Self>) {
        let mut show = false;
        let mut activate = false;
        let mut notices = Vec::new();
        let mut snapshot: Option<String> = None;
        let mut folders: Option<String> = None;
        let mut progress: Option<String> = None;
        let mut estimate: Option<String> = None;
        let mut status_at: Option<(i32, i32)> = None;
        {
            let mut rust = self.as_mut().rust_mut();
            let Some(rt) = rust.runtime.as_mut() else {
                return;
            };
            if let Some(rx) = rt.tray_rx.as_ref() {
                for event in rx.try_iter() {
                    match event {
                        TrayEvent::Open => show = true,
                        TrayEvent::Status(x, y) => status_at = Some((x, y)),
                    }
                }
            }
            if rt
                .activations
                .as_ref()
                .is_some_and(|rx| rx.try_iter().count() > 0)
            {
                show = true;
                activate = true;
            }
            for notice in rt.notices.try_iter() {
                match notice {
                    Notice::Folders(text) => folders = Some(text),
                    Notice::Estimate(text) => estimate = Some(text),
                    Notice::Dialog { kind, title, body } => {
                        if kind == "login" || kind == "fatal" {
                            show = true;
                        }
                        notices.push((kind, title, body));
                    }
                }
            }
            let (generation, json) = rt.shared.bus.load();
            if generation != rt.seen_gen {
                rt.seen_gen = generation;
                if !rt.saw_snapshot {
                    rt.saw_snapshot = true;
                    let empty = json.contains("\"remotes\":[]");
                    if empty || !rt.background {
                        show = true;
                    }
                }
                snapshot = Some(json);
            }
            let generation = rt.shared.progress.generation();
            if generation != rt.seen_progress {
                rt.seen_progress = generation;
                progress = Some(rt.shared.progress.json());
            }
        }
        if show {
            self.as_mut().set_window_visible(true);
        }
        if activate {
            self.as_mut().activated();
        }
        if let Some(text) = folders {
            self.as_mut().set_folders_json(QString::from(&*text));
        }
        if let Some(text) = snapshot {
            self.as_mut().set_snapshot_json(QString::from(&*text));
        }
        if let Some(text) = progress {
            self.as_mut().set_progress_json(QString::from(&*text));
        }
        if let Some(text) = estimate {
            self.as_mut().set_estimate_json(QString::from(&*text));
        }
        if let Some((x, y)) = status_at {
            self.as_mut().tray_activated(x, y);
        }
        for (kind, title, body) in notices {
            self.as_mut().notice(
                QString::from(&*kind),
                QString::from(&*title),
                QString::from(&*body),
            );
        }
    }

    fn hide_window(mut self: Pin<&mut Self>) {
        self.as_mut().set_window_visible(false);
    }

    fn show_window(mut self: Pin<&mut Self>) {
        self.as_mut().set_window_visible(true);
    }

    /// Quit at once, even mid-transfer; see [`crate::exit`].
    fn quit(self: Pin<&mut Self>) {
        crate::exit::now(0);
    }

    fn submit_login(self: Pin<&mut Self>, payload: &QString) {
        let Ok(req) = login_from_json(&payload.to_string()) else {
            return;
        };
        self.send(Command::SubmitLogin(req));
    }

    fn cancel_auth(self: Pin<&mut Self>) {
        if let Some(rt) = self.rust().runtime.as_ref() {
            rt.shared.cancel_auth.store(true, Ordering::Relaxed);
        }
    }

    fn delete_remote(self: Pin<&mut Self>, name: &QString) {
        self.send(Command::DeleteRemote(name.to_string()));
    }

    fn stop_dir(self: Pin<&mut Self>, remote: &QString, local: &QString, remote_path: &QString) {
        self.send(Command::StopDir {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
        });
    }

    fn add_dir(self: Pin<&mut Self>, remote: &QString, local: &QString, remote_path: &QString) {
        self.send(Command::AddDir {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
        });
    }

    fn resume_dir(self: Pin<&mut Self>, remote: &QString, local: &QString, remote_path: &QString) {
        self.send(Command::ResumeDir {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
        });
    }

    fn save_exclusions(self: Pin<&mut Self>, local: &QString, text: &QString) {
        self.send(Command::SaveExclusions {
            local: local.to_string(),
            text: text.to_string(),
        });
    }

    fn activate_error(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
        id: i64,
    ) {
        self.send(Command::ActivateError {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
            id: id as u64,
        });
    }

    fn confirm_deletion(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
        id: i64,
        token: &QString,
    ) {
        self.send(Command::ConfirmDeletion {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
            id: id as u64,
            token: token.to_string(),
        });
    }

    fn restore_version(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
        id: i64,
    ) {
        self.send(Command::Restore {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
            recovery_id: id,
        });
    }

    fn delete_version(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
        id: i64,
    ) {
        self.send(Command::DiscardVersion {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
            recovery_id: id,
        });
    }

    fn dismiss_error(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
        id: i64,
    ) {
        self.send(Command::DismissError {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
            id: id as u64,
        });
    }

    fn estimate_dir(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
    ) {
        if let Some(rt) = self.rust().runtime.as_ref() {
            sync::measure::request(
                &rt.shared,
                remote.to_string(),
                local.to_string(),
                remote_path.to_string(),
            );
        }
    }

    fn request_stats(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
    ) {
        self.send(Command::StatsDir {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
        });
    }

    fn cancel_estimate(self: Pin<&mut Self>) {
        sync::measure::cancel();
    }

    fn list_dirs(self: Pin<&mut Self>, remote: &QString, path: &QString) {
        self.send(Command::ListDirs {
            remote: remote.to_string(),
            path: path.to_string(),
        });
    }

    fn pick_local_folder(self: Pin<&mut Self>, start: &QString) {
        static OPEN: AtomicBool = AtomicBool::new(false);
        let Some(rt) = self.rust().runtime.as_ref() else {
            return;
        };
        if OPEN.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = rt.shared.clone();
        let start = start.to_string();
        let start = if start.trim().is_empty() {
            std::env::var_os("HOME").map(std::path::PathBuf::from)
        } else {
            Some(std::path::PathBuf::from(start.trim()))
        };
        let spawned = std::thread::Builder::new()
            .name("folder-chooser".into())
            .spawn(move || {
                use crate::platform::portal::{Picked, pick_folder};
                match pick_folder("Choose a local folder", start.as_deref()) {
                    Ok(Picked::Folder(path)) => {
                        shared.dialog("local-folder", "", path.to_string_lossy())
                    }
                    Ok(Picked::Cancelled) => {}
                    Err(err) => {
                        tracing::info!("portal folder chooser unavailable: {err}");
                        shared.dialog("local-folder-fallback", "", "");
                    }
                }
                OPEN.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            OPEN.store(false, Ordering::Release);
        }
    }
}

impl qobject::AppController {
    fn send(self: Pin<&mut Self>, command: Command) {
        if let Some(rt) = self.rust().runtime.as_ref() {
            let _ = rt.commands.send(command);
        }
    }
}

fn login_from_json(text: &str) -> Result<LoginRequest, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let field = |key: &str| {
        value
            .get(key)
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    Ok(LoginRequest {
        kind: field("kind"),
        name: field("name"),
        url: field("url"),
        username: field("username"),
        password: value
            .get("password")
            .and_then(|item| item.as_str())
            .unwrap_or("")
            .to_owned(),
        totp: field("totp"),
        totp_enabled: value
            .get("totpEnabled")
            .and_then(|item| item.as_bool())
            .unwrap_or(false),
    })
}
