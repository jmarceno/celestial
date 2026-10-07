//! StatusNotifier tray, using the embedded logo with a colored status badge.

use crossbeam_channel::{Receiver, Sender, unbounded};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray, TrayService};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::branding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Open,
    Quit,
}

struct CelestialTray {
    tx: Sender<TrayEvent>,
    message: String,
    phase: String,
    icons: Vec<Icon>,
}

impl CelestialTray {
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
        self.icons
            .iter()
            .map(|icon| status_icon(icon, &self.phase))
            .collect()
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
    handle: ksni::Handle<CelestialTray>,
}

impl TrayHandle {
    pub fn start() -> Result<Self, String> {
        if !wait_for_watcher() {
            return Err(
                "no StatusNotifierWatcher on the session bus (system tray host missing)".into(),
            );
        }
        let (tx, rx) = unbounded();
        let icons = [32, 64]
            .into_iter()
            .map(|size| {
                branding::logo_argb(size).map(|data| Icon {
                    width: size,
                    height: size,
                    data,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let service = TrayService::new(CelestialTray {
            tx,
            message: tr::tr!("Awaiting sync checks..."),
            phase: "loading".into(),
            icons,
        });
        let handle = service.handle();
        thread::Builder::new()
            .name("celestial-tray".into())
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
