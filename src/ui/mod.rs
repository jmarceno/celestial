//! GUI entry point: single-instance check, tray, Qt environment, QML load.

pub(crate) mod branding;
pub mod controller;
pub mod tray;

use std::sync::Arc;
use std::time::Duration;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQuickStyle, QString, QUrl};

use crate::single_instance::{self, AcquireOutcome};
use crate::ui::controller::{UiBootstrap, install_bootstrap};
use crate::ui::tray::TrayHandle;

const MAIN_QML: &str = "qrc:/qt/qml/app/celestial/qml/Main.qml";

pub fn run(background: bool) -> i32 {
    crate::util::log_to_file();
    tracing::info!("Celestial {} started", env!("CARGO_PKG_VERSION"));
    let guard = match single_instance::acquire() {
        Ok(AcquireOutcome::Secondary) => {
            eprintln!("Celestial is already running; opened the existing window.");
            return 0;
        }
        Ok(AcquireOutcome::Primary(guard)) => Some(guard),
        Err(err) => {
            tracing::warn!("single-instance check failed (continuing): {err}");
            None
        }
    };

    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        eprintln!("No graphical session (DISPLAY/WAYLAND_DISPLAY unset).");
        return 1;
    }

    crate::exit::on_signals();
    crate::platform::desktop::refresh_stale_entries();

    if std::env::var_os("QT_FORCE_STDERR_LOGGING").is_none() {
        // SAFETY: main thread, before any Qt object exists.
        unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    }
    apply_render_backend_fallback();

    let skip_tray = std::env::var_os("CELESTIAL_FAKE_TRAY").is_some()
        || std::env::var("QT_QPA_PLATFORM").ok().as_deref() == Some("offscreen");
    let tray = if skip_tray {
        None
    } else {
        match TrayHandle::start() {
            Ok(tray) => Some(Arc::new(tray)),
            Err(err) => {
                eprintln!("Celestial cannot start without a system tray: {err}");
                return 1;
            }
        }
    };
    let tray_rx = tray.as_ref().map(|tray| tray.rx.clone());

    install_bootstrap(UiBootstrap {
        activations: guard.as_ref().map(|guard| guard.receiver()),
        tray,
        tray_rx,
        background,
    });
    spawn_bootstrap_watchdog();

    QQuickStyle::set_style(&QString::from("Basic"));

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(app) = app.as_mut() {
        app.set_application_name(&QString::from(crate::util::APP_NAME));
        QGuiApplication::set_desktop_file_name(&QString::from(crate::util::APP_ID));
        if let Err(err) = branding::set_window_icon() {
            tracing::warn!("could not set window icon: {err}");
        }
    }
    let Some(engine) = engine.as_mut() else {
        eprintln!("Celestial failed to start: Qt QML engine is null.");
        return 1;
    };
    engine.load(&QUrl::from(MAIN_QML));

    let code = app.as_mut().map(|app| app.exec()).unwrap_or(1);
    drop(guard);
    // The event loop also ends when the session closes: leave at once
    // rather than wait on the sync; see `crate::exit`.
    crate::exit::now(code)
}

fn apply_render_backend_fallback() {
    if std::env::var_os("QT_QUICK_BACKEND").is_none()
        && std::env::var_os("QSG_RHI_BACKEND").is_none()
    {
        // SAFETY: main thread, before any Qt object exists.
        unsafe { std::env::set_var("QT_QUICK_BACKEND", "software") };
    }
}

fn spawn_bootstrap_watchdog() {
    let _ = std::thread::Builder::new()
        .name("qml-watchdog".into())
        .spawn(|| {
            std::thread::sleep(Duration::from_secs(10));
            if controller::is_bootstrap_pending() {
                eprintln!(
                    "Celestial failed to start: the QML UI did not load.\n\
                     See the QML error above, or `journalctl --user --since '5 minutes ago' | grep -i qml`."
                );
                std::process::exit(2);
            }
        });
}
