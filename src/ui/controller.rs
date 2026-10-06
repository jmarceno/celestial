//! Rust <-> QML bridge.
//!
//! Invokables and `tick()` run on the Qt thread. They only enqueue work.
//! The sync worker owns rclone and the database.

use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

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
        #[qproperty(QString, suggestions_json)]
        #[qproperty(bool, window_visible)]
        #[qproperty(bool, quit_requested)]
        type AppController = super::AppControllerRust;

        #[qsignal]
        fn activated(self: Pin<&mut Self>);

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
        #[cxx_name = "chooseConflict"]
        fn choose_conflict(
            self: Pin<&mut Self>,
            remote: &QString,
            local: &QString,
            remote_path: &QString,
            id: i64,
            keep_local: bool,
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

        #[qinvokable]
        #[cxx_name = "listDirs"]
        fn list_dirs(self: Pin<&mut Self>, remote: &QString, path: &QString);
    }
}

struct Runtime {
    shared: Shared,
    commands: crossbeam_channel::Sender<Command>,
    notices: crossbeam_channel::Receiver<Notice>,
    tray_rx: Option<crossbeam_channel::Receiver<TrayEvent>>,
    activations: Option<crossbeam_channel::Receiver<Activation>>,
    seen_gen: u64,
    background: bool,
    saw_snapshot: bool,
    exit_requested: bool,
}

pub struct AppControllerRust {
    snapshot_json: QString,
    suggestions_json: QString,
    window_visible: bool,
    quit_requested: bool,
    runtime: Option<Runtime>,
}

impl Default for AppControllerRust {
    fn default() -> Self {
        Self {
            snapshot_json: QString::from("{}"),
            suggestions_json: QString::from("[]"),
            window_visible: false,
            quit_requested: false,
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
        let stopped = Arc::new(AtomicBool::new(false));
        let cancel_auth = Arc::new(AtomicBool::new(false));
        let tray = boot.tray.clone();
        let shared = Shared {
            bus: crate::sync::model::SnapshotBus::new(),
            quit: Arc::new(AtomicBool::new(false)),
            stopped,
            cancel_auth,
            notices: notice_tx,
            tray: Arc::new(move |message, phase| {
                if let Some(tray) = tray.as_ref() {
                    tray.update(message, phase);
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
            background: boot.background,
            saw_snapshot: false,
            exit_requested: false,
        });
        self.as_mut().set_window_visible(!boot.background);
        tracing::info!("QML bootstrap complete");
    }

    fn tick(mut self: Pin<&mut Self>) {
        let mut show = false;
        let mut activate = false;
        let mut notices = Vec::new();
        let mut snapshot: Option<String> = None;
        let mut suggestions: Option<String> = None;
        let mut exit_now = false;
        {
            let mut rust = self.as_mut().rust_mut();
            let Some(rt) = rust.runtime.as_mut() else {
                return;
            };
            if let Some(rx) = rt.tray_rx.as_ref() {
                for event in rx.try_iter() {
                    match event {
                        TrayEvent::Open => show = true,
                        TrayEvent::Quit => {
                            rt.exit_requested = true;
                            rt.shared.quit.store(true, Ordering::Relaxed);
                            let _ = rt.commands.send(Command::Quit);
                        }
                    }
                }
            }
            if rt.activations.as_ref().is_some_and(|rx| rx.try_iter().count() > 0) {
                show = true;
                activate = true;
            }
            for notice in rt.notices.try_iter() {
                match notice {
                    Notice::Suggestions(text) => suggestions = Some(text),
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
            if rt.exit_requested && rt.shared.stopped.load(Ordering::Relaxed) {
                exit_now = true;
            }
        }
        if show {
            self.as_mut().set_window_visible(true);
        }
        if activate {
            self.as_mut().activated();
        }
        if let Some(text) = suggestions {
            self.as_mut().set_suggestions_json(QString::from(&*text));
        }
        if let Some(text) = snapshot {
            self.as_mut().set_snapshot_json(QString::from(&*text));
        }
        for (kind, title, body) in notices {
            self.as_mut().notice(
                QString::from(&*kind),
                QString::from(&*title),
                QString::from(&*body),
            );
        }
        if exit_now {
            self.as_mut().set_quit_requested(true);
            std::process::exit(0);
        }
    }

    fn hide_window(mut self: Pin<&mut Self>) {
        self.as_mut().set_window_visible(false);
    }

    fn show_window(mut self: Pin<&mut Self>) {
        self.as_mut().set_window_visible(true);
    }

    fn quit(mut self: Pin<&mut Self>) {
        let mut rust = self.as_mut().rust_mut();
        let Some(rt) = rust.runtime.as_mut() else {
            std::process::exit(0);
        };
        rt.exit_requested = true;
        rt.shared.quit.store(true, Ordering::Relaxed);
        let _ = rt.commands.send(Command::Quit);
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

    fn choose_conflict(
        self: Pin<&mut Self>,
        remote: &QString,
        local: &QString,
        remote_path: &QString,
        id: i64,
        keep_local: bool,
    ) {
        self.send(Command::ChooseConflict {
            remote: remote.to_string(),
            local: local.to_string(),
            remote_path: remote_path.to_string(),
            id: id as u64,
            keep_local,
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

    fn list_dirs(self: Pin<&mut Self>, remote: &QString, path: &QString) {
        self.send(Command::ListDirs {
            remote: remote.to_string(),
            path: path.to_string(),
        });
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
