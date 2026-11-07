//! StatusNotifier tray, using the embedded logo with a colored status badge.

use crossbeam_channel::{Receiver, Sender, unbounded};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray, TrayService};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::branding;

/// How long the first registration gets to fail before the tray is declared up.
const FIRST_ATTEMPT_WINDOW: Duration = Duration::from_millis(800);
/// Pause between attempts when the session bus dropped the tray connection.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    /// The icon was clicked at these screen coordinates (0, 0 when the
    /// host does not say): show the status window.
    Status(i32, i32),
}

struct CelestialTray {
    tx: Sender<TrayEvent>,
    /// Shared with `TrayHandle`: a reconnected `TrayService` reads the current
    /// message/phase from here instead of the values it was built with, so the
    /// icon never reverts to the startup text after a bus hiccup.
    shared: Arc<Mutex<TrayState>>,
    icons: Arc<Vec<Icon>>,
}

/// The parts of the tray that survive a new `TrayService`.
#[derive(Debug, Clone)]
struct TrayState {
    message: String,
    phase: String,
    /// Transfer progress in total, while something is transferred.
    progress: Option<String>,
}

impl CelestialTray {
    fn state(&self) -> TrayState {
        // A poisoned lock still holds the last known state; a tray must never
        // panic on the D-Bus thread just because a UI thread died holding it.
        self.shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn send(&self, event: TrayEvent) {
        let _ = self.tx.send(event);
    }
}

impl Tray for CelestialTray {
    fn id(&self) -> String {
        crate::util::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "Celestial".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        let phase = self.state().phase;
        self.icons
            .iter()
            .map(|icon| status_icon(icon, &phase))
            .collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let state = self.state();
        ToolTip {
            title: state.message.clone(),
            description: state.progress.clone().unwrap_or_default(),
            icon_name: String::new(),
            icon_pixmap: self.icon_pixmap(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let state = self.state();
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: state.message.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];
        // Some hosts show this menu on a left click instead of activating
        // the icon, so it carries the progress too.
        if let Some(progress) = &state.progress {
            items.push(
                StandardItem {
                    label: progress.clone(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }
        items.extend([
            StandardItem {
                label: tr::tr!("Open"),
                activate: Box::new(|tray: &mut Self| tray.send(TrayEvent::Open)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tr::tr!("Close"),
                // Quit right here on the tray thread, without waiting for
                // the window's event loop or the sync; see `crate::exit`.
                activate: Box::new(|_: &mut Self| crate::exit::now(0)),
                ..Default::default()
            }
            .into(),
        ]);
        items
    }

    fn activate(&mut self, x: i32, y: i32) {
        self.send(TrayEvent::Status(x, y));
    }

    /// The watcher only blips (a restart, or a session-bus reconnect). Returning
    /// `false` here makes ksni shut the service down for good, so keep it alive:
    /// ksni re-registers by itself when the watcher name comes back.
    fn watcher_offine(&self) -> bool {
        tracing::warn!("system tray host went away; waiting for it to come back");
        true
    }
}

pub struct TrayHandle {
    pub rx: Receiver<TrayEvent>,
    /// Swapped for a new handle every time the service reconnects, so
    /// `update()` always reaches the `TrayService` that is actually running.
    handle: Arc<Mutex<ksni::Handle<CelestialTray>>>,
    state: Arc<Mutex<TrayState>>,
    stop: Arc<AtomicBool>,
}

impl TrayHandle {
    pub fn start() -> Result<Self, String> {
        if !wait_for_watcher() {
            return Err(
                "no StatusNotifierWatcher on the session bus (system tray host missing)".into(),
            );
        }
        let (tx, rx) = unbounded();
        let icons = Arc::new(render_icons()?);
        let state = Arc::new(Mutex::new(TrayState {
            message: tr::tr!("Awaiting sync checks..."),
            phase: "loading".into(),
            progress: None,
        }));

        let service = build_service(tx.clone(), state.clone(), icons.clone());
        let handle = Arc::new(Mutex::new(service.handle()));
        let stop = Arc::new(AtomicBool::new(false));

        let (report_tx, report_rx) = std::sync::mpsc::channel();
        {
            let thread_handle = handle.clone();
            let thread_state = state.clone();
            let thread_stop = stop.clone();
            thread::Builder::new()
                .name("celestial-tray".into())
                .spawn(move || {
                    // One service per session-bus connection. ksni returns an
                    // error when the connection dies and never retries, so a
                    // new connection comes from a new `TrayService`.
                    let mut next = Some(service);
                    let mut reported = false;
                    while !thread_stop.load(Ordering::Acquire) {
                        let Some(service) = next.take() else {
                            break;
                        };
                        match service.run() {
                            // A clean stop is a real shutdown, not a reconnect.
                            Ok(()) => break,
                            Err(err) => {
                                if !reported {
                                    reported = true;
                                    let _ = report_tx.send(err.to_string());
                                }
                                tracing::warn!("tray connection lost ({err}); reconnecting");
                            }
                        }
                        if thread_stop.load(Ordering::Acquire) {
                            break;
                        }
                        thread::sleep(RECONNECT_INTERVAL);
                        let rebuilt = build_service(
                            tx.clone(),
                            thread_state.clone(),
                            icons.clone(),
                        );
                        *thread_handle
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = rebuilt.handle();
                        next = Some(rebuilt);
                    }
                })
                .map_err(|err| format!("failed to start tray thread: {err}"))?;
        }

        match report_rx.recv_timeout(FIRST_ATTEMPT_WINDOW) {
            Ok(err) => return Err(format!("could not create the tray icon: {err}")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("tray thread exited before registering".into());
            }
            // Still running: the first registration succeeded.
            Err(RecvTimeoutError::Timeout) => {}
        }

        Ok(Self {
            rx,
            handle,
            state,
            stop,
        })
    }

    pub fn update(self: &Arc<Self>, message: String, phase: String) {
        let snapshot = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.message = message;
            state.phase = phase;
            state.clone()
        };
        self.push(snapshot);
    }

    pub fn update_progress(self: &Arc<Self>, progress: Option<String>) {
        let snapshot = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.progress = progress;
            state.clone()
        };
        self.push(snapshot);
    }

    fn push(&self, snapshot: TrayState) {
        self.handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .update(|tray| {
                *tray
                    .shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
            });
    }
}

impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(handle) = self.handle.lock() {
            handle.shutdown();
        }
    }
}

fn build_service(
    tx: Sender<TrayEvent>,
    shared: Arc<Mutex<TrayState>>,
    icons: Arc<Vec<Icon>>,
) -> TrayService<CelestialTray> {
    TrayService::new(CelestialTray {
        tx,
        shared,
        icons,
    })
}

fn render_icons() -> Result<Vec<Icon>, String> {
    [32, 64]
        .into_iter()
        .map(|size| {
            branding::logo_argb(size).map(|data| Icon {
                width: size,
                height: size,
                data,
            })
        })
        .collect::<Result<Vec<_>, _>>()
}

fn wait_for_watcher() -> bool {
    if std::env::var_os("CELESTIAL_FAKE_TRAY").is_some()
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
    if std::env::var_os("CELESTIAL_FAKE_TRAY").is_some()
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

fn status_icon(base: &Icon, phase: &str) -> Icon {
    let (r, g, b) = match phase {
        "syncing" => (0x2f, 0xb3, 0xa3),
        "done" => (0x3e, 0xcf, 0x8e),
        "warning" => (0xe8, 0xb3, 0x39),
        "disconnected" => (0xe5, 0x65, 0x6e),
        _ => (0xc5, 0xc9, 0xce),
    };
    let size = base.width;
    let mut icon = base.clone();
    let center = size as f32 * 0.81;
    let radius = size as f32 * 0.105;
    let halo = size as f32 * 0.045;
    for y in 0..size {
        for x in 0..size {
            let distance =
                ((x as f32 + 0.5 - center).powi(2) + (y as f32 + 0.5 - center).powi(2)).sqrt();
            let coverage = (radius + halo + 0.5 - distance).clamp(0.0, 1.0);
            if coverage == 0.0 {
                continue;
            }
            let inner = (radius + 0.5 - distance).clamp(0.0, 1.0);
            let i = ((y * size + x) * 4) as usize;
            let alpha = icon.data[i] as f32 / 255.0;
            let out_alpha = coverage + alpha * (1.0 - coverage);
            for (channel, color, background) in [(1, r, 0x12), (2, g, 0x14), (3, b, 0x18)] {
                let badge = background as f32 + (color as f32 - background as f32) * inner;
                icon.data[i + channel] = ((badge * coverage
                    + icon.data[i + channel] as f32 * alpha * (1.0 - coverage))
                    / out_alpha)
                    .round() as u8;
            }
            icon.data[i] = (out_alpha * 255.0).round() as u8;
        }
    }
    icon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_tray_icons_keep_transparency_and_brand_across_statuses() {
        for size in [32, 64] {
            let base = Icon {
                width: size,
                height: size,
                data: branding::logo_argb(size).expect("SVG must render"),
            };
            assert_eq!(base.data.len(), (size * size * 4) as usize);
            assert_eq!(&base.data[..4], &[0, 0, 0, 0]);
            assert!(base.data.chunks_exact(4).any(|pixel| pixel[0] == 255));
            assert!(
                base.data
                    .chunks_exact(4)
                    .any(|pixel| pixel[0] > 0 && pixel[0] < 255)
            );
            for (phase, color) in [
                ("syncing", [0x2f, 0xb3, 0xa3]),
                ("done", [0x3e, 0xcf, 0x8e]),
                ("warning", [0xe8, 0xb3, 0x39]),
                ("disconnected", [0xe5, 0x65, 0x6e]),
                ("loading", [0xc5, 0xc9, 0xce]),
            ] {
                let icon = status_icon(&base, phase);
                let center = (size as f32 * 0.81) as usize;
                let badge = (center * size as usize + center) * 4;
                assert_eq!(
                    &icon.data[badge..badge + 4],
                    &[255, color[0], color[1], color[2]]
                );
                // The badge never changes the upper half or central star.
                let unchanged = (size * size * 4 * 2 / 3) as usize;
                assert_eq!(&icon.data[..unchanged], &base.data[..unchanged]);
            }
        }
        assert!(branding::logo_argb(0).is_err());
        assert!(branding::logo_argb(257).is_err());
    }
}
