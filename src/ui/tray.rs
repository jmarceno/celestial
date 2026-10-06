//! StatusNotifier tray. The icon is drawn here so it does not depend on a
//! theme having Celeste's old symbolic names installed.

use crossbeam_channel::{Receiver, Sender, unbounded};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray, TrayService};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    Quit,
}

struct CelesteTray {
    tx: Sender<TrayEvent>,
    message: String,
    phase: String,
}

impl CelesteTray {
    fn send(&self, event: TrayEvent) {
        let _ = self.tx.send(event);
    }
}

impl Tray for CelesteTray {
    fn id(&self) -> String {
        crate::util::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "Celeste".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![draw_icon(&self.phase)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.message.clone(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: self.icon_pixmap(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: self.message.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tr::tr!("Open"),
                activate: Box::new(|tray: &mut Self| tray.send(TrayEvent::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tr::tr!("Close"),
                activate: Box::new(|tray: &mut Self| tray.send(TrayEvent::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayEvent::Open);
    }

    fn watcher_offine(&self) -> bool {
        false
    }
}

pub struct TrayHandle {
    pub rx: Receiver<TrayEvent>,
    handle: ksni::Handle<CelesteTray>,
}

impl TrayHandle {
    pub fn start() -> Result<Self, String> {
        if !wait_for_watcher() {
            return Err(
                "no StatusNotifierWatcher on the session bus (system tray host missing)".into(),
            );
        }
        let (tx, rx) = unbounded();
        let service = TrayService::new(CelesteTray {
            tx,
            message: tr::tr!("Awaiting sync checks..."),
            phase: "loading".into(),
        });
        let handle = service.handle();
        thread::Builder::new()
            .name("celeste-tray".into())
            .spawn(move || {
                if let Err(err) = service.run() {
                    tracing::error!("tray stopped: {err}");
                }
            })
            .map_err(|err| format!("failed to start tray thread: {err}"))?;
        Ok(Self { rx, handle })
    }

    pub fn update(self: &Arc<Self>, message: String, phase: String) {
        self.handle.update(|tray| {
            tray.message = message;
            tray.phase = phase;
        });
    }
}

fn wait_for_watcher() -> bool {
    if std::env::var_os("CELESTE_FAKE_TRAY").is_some()
        || std::env::var("QT_QPA_PLATFORM").ok().as_deref() == Some("offscreen")
    {
        return true;
    }
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut waited = false;
    while Instant::now() < deadline {
        if watcher_present() {
            return true;
        }
        if !waited {
            tracing::info!("waiting up to 120 s for the system tray host");
            waited = true;
        }
        thread::sleep(Duration::from_millis(500));
    }
    watcher_present()
}

fn watcher_present() -> bool {
    if std::env::var_os("CELESTE_FAKE_TRAY").is_some()
        || std::env::var("QT_QPA_PLATFORM").ok().as_deref() == Some("offscreen")
    {
        return true;
    }
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return false;
    };
    let Ok(dbus) = zbus::blocking::fdo::DBusProxy::new(&conn) else {
        return false;
    };
    for name in [
        "org.kde.StatusNotifierWatcher",
        "org.freedesktop.StatusNotifierWatcher",
    ] {
        let Ok(bus_name) = zbus::names::BusName::try_from(name) else {
            continue;
        };
        if dbus.name_has_owner(bus_name).unwrap_or(false) {
            return true;
        }
    }
    false
}

fn draw_icon(phase: &str) -> Icon {
    let (r, g, b) = match phase {
        "syncing" => (0x2f, 0xb3, 0xa3),
        "done" => (0x3e, 0xcf, 0x8e),
        "warning" => (0xe8, 0xb3, 0x39),
        "disconnected" => (0xe5, 0x65, 0x6e),
        _ => (0xc5, 0xc9, 0xce),
    };
    let size = 32i32;
    let mut data = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let xf = x as f32;
            let yf = y as f32;
            let cloud = (xf - 16.0).powi(2) / 110.0 + (yf - 18.0).powi(2) / 58.0 <= 1.0
                || (xf - 11.0).powi(2) + (yf - 14.0).powi(2) <= 36.0
                || (xf - 21.0).powi(2) + (yf - 13.0).powi(2) <= 28.0;
            if !cloud {
                continue;
            }
            let i = ((y * size + x) * 4) as usize;
            data[i] = 255;
            data[i + 1] = r;
            data[i + 2] = g;
            data[i + 3] = b;
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}
