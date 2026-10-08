//! Live transfer progress: what a pass plans to transfer, how much of it is
//! done, the speed and the time left.
//!
//! A pass copies one file per `operations/copyfile` call, so rclone only
//! knows the file in flight. The worker reports the pass plan and each
//! finished item; a meter thread reads rclone's stats group for the bytes in
//! flight. The window reads the result as its own JSON, so the snapshot is
//! not rebuilt every second.

use super::Shared;
use crate::rclone::{self, GroupStats};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// What one pass is about to transfer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub upload_files: u64,
    pub upload_bytes: u64,
    pub download_files: u64,
    pub download_bytes: u64,
    /// Files and folders to delete.
    pub removals: u64,
    /// Files whose size the remote does not report; not in the byte totals.
    pub unknown_size: u64,
}

impl Plan {
    pub fn files(&self) -> u64 {
        self.upload_files + self.download_files
    }

    pub fn bytes(&self) -> u64 {
        self.upload_bytes + self.download_bytes
    }
}

/// The rclone stats group a pair's transfers are counted in.
pub fn group_name(sync_dir_id: i32) -> String {
    format!("celestial/pair-{sync_dir_id}")
}

/// How long the speed takes to follow a change (seconds).
const SMOOTHING: f64 = 10.0;
/// No time left is shown before the speed has been measured this long.
const WARMUP: Duration = Duration::from_secs(3);
const METER_INTERVAL: Duration = Duration::from_secs(1);
/// The tray is a D-Bus round trip: update its progress line less often.
const TRAY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct PairProgress {
    pub remote: String,
    pub local: String,
    pub remote_path: String,
    pub plan: Plan,
    pub done_files: u64,
    /// Planned bytes of finished items, whether they succeeded or not.
    pub done_bytes: u64,
    /// The item being transferred.
    pub current: Option<String>,
    /// Bytes of the transfer in flight.
    pub in_flight: u64,
    /// Bytes per second over wall-clock time, so the time spent between
    /// files counts too.
    pub speed: f64,
    started: Instant,
    measured: bool,
    sample: Option<(Instant, u64)>,
}

impl PairProgress {
    fn new(remote: &str, local: &str, remote_path: &str, plan: Plan, now: Instant) -> Self {
        Self {
            remote: remote.to_owned(),
            local: local.to_owned(),
            remote_path: remote_path.to_owned(),
            plan,
            done_files: 0,
            done_bytes: 0,
            current: None,
            in_flight: 0,
            speed: 0.0,
            started: now,
            measured: false,
            sample: None,
        }
    }

    pub fn transferred(&self) -> u64 {
        self.done_bytes
            .saturating_add(self.in_flight)
            .min(self.plan.bytes())
    }

    pub fn remaining(&self) -> u64 {
        self.plan.bytes() - self.transferred()
    }

    /// Seconds left, once the speed is known.
    pub fn eta_secs(&self, now: Instant) -> Option<u64> {
        eta(
            self.remaining(),
            self.speed,
            now.duration_since(self.started),
        )
    }

    /// Fold one reading of the pair's stats group into the speed.
    pub fn sample(&mut self, at: Instant, stats: GroupStats) {
        self.in_flight = stats.in_flight;
        if let Some((then, bytes)) = self.sample {
            let dt = at.duration_since(then).as_secs_f64();
            if dt > 0.0 && stats.bytes >= bytes {
                let rate = (stats.bytes - bytes) as f64 / dt;
                if self.measured {
                    let weight = 1.0 - (-dt / SMOOTHING).exp();
                    self.speed += weight * (rate - self.speed);
                } else {
                    self.speed = rate;
                    self.measured = true;
                }
            }
        }
        self.sample = Some((at, stats.bytes));
    }
}

fn eta(remaining: u64, speed: f64, elapsed: Duration) -> Option<u64> {
    if remaining == 0 {
        return Some(0);
    }
    if elapsed < WARMUP || speed < 1.0 {
        return None;
    }
    Some((remaining as f64 / speed).ceil() as u64)
}

/// Progress of the pairs being synchronized, shared by the worker, the meter
/// and the window.
#[derive(Clone, Default)]
pub struct ProgressBus {
    pairs: Arc<Mutex<BTreeMap<i32, PairProgress>>>,
    generation: Arc<AtomicU64>,
}

impl ProgressBus {
    fn edit<R>(&self, edit: impl FnOnce(&mut BTreeMap<i32, PairProgress>) -> R) -> R {
        let result = edit(&mut self.pairs.lock().unwrap_or_else(|err| err.into_inner()));
        self.generation.fetch_add(1, Ordering::Relaxed);
        result
    }

    /// A pass of `id` starts transferring `plan`. A plan with nothing to
    /// transfer shows no progress.
    pub fn start(&self, id: i32, remote: &str, local: &str, remote_path: &str, plan: Plan) {
        self.edit(|pairs| {
            if plan.files() == 0 {
                pairs.remove(&id);
            } else {
                pairs.insert(
                    id,
                    PairProgress::new(remote, local, remote_path, plan, Instant::now()),
                );
            }
        });
    }

    pub fn current(&self, id: i32, rel: &str) {
        self.edit(|pairs| {
            if let Some(pair) = pairs.get_mut(&id) {
                pair.current = Some(rel.to_owned());
            }
        });
    }

    /// A planned transfer of `files` files and `bytes` bytes finished,
    /// successfully or not.
    pub fn finished(&self, id: i32, files: u64, bytes: u64) {
        self.edit(|pairs| {
            if let Some(pair) = pairs.get_mut(&id) {
                pair.done_files = (pair.done_files + files).min(pair.plan.files());
                pair.done_bytes = pair.done_bytes.saturating_add(bytes);
                pair.in_flight = 0;
                pair.current = None;
            }
        });
    }

    pub fn end(&self, id: i32) {
        self.edit(|pairs| pairs.remove(&id));
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub fn get(&self, id: i32) -> Option<PairProgress> {
        self.pairs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&id)
            .cloned()
    }

    /// Read every active pair's stats group. The lock is not held during
    /// the RPCs. Returns whether any pair is active.
    pub fn sample_all(&self, read: impl Fn(&str) -> Option<GroupStats>) -> bool {
        let ids: Vec<i32> = self
            .pairs
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .keys()
            .copied()
            .collect();
        if ids.is_empty() {
            return false;
        }
        let readings: Vec<(i32, GroupStats)> = ids
            .into_iter()
            .filter_map(|id| read(&group_name(id)).map(|stats| (id, stats)))
            .collect();
        let now = Instant::now();
        self.edit(|pairs| {
            for (id, stats) in readings {
                if let Some(pair) = pairs.get_mut(&id) {
                    pair.sample(now, stats);
                }
            }
        });
        true
    }

    pub fn json(&self) -> String {
        let pairs = self.pairs.lock().unwrap_or_else(|err| err.into_inner());
        let now = Instant::now();
        let list: Vec<Value> = pairs
            .values()
            .map(|pair| {
                let mut value = Totals::of(pair, now).json();
                value["remote"] = json!(pair.remote);
                value["localPath"] = json!(pair.local);
                value["remotePath"] = json!(pair.remote_path);
                value["current"] = json!(pair.current);
                value
            })
            .collect();
        let total = (!pairs.is_empty()).then(|| Totals::sum(pairs.values(), now).json());
        json!({ "pairs": list, "total": total }).to_string()
    }

    /// One line for the tray, `None` when nothing is being transferred.
    pub fn summary(&self) -> Option<String> {
        let pairs = self.pairs.lock().unwrap_or_else(|err| err.into_inner());
        (!pairs.is_empty()).then(|| Totals::sum(pairs.values(), Instant::now()).text())
    }
}

/// Progress of one pair or the sum of several.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Totals {
    pub plan: Plan,
    pub done_files: u64,
    pub done_bytes: u64,
    pub speed: f64,
    pub eta: Option<u64>,
}

impl Totals {
    fn of(pair: &PairProgress, now: Instant) -> Self {
        Self {
            plan: pair.plan,
            done_files: pair.done_files,
            done_bytes: pair.transferred(),
            speed: pair.speed,
            eta: pair.eta_secs(now),
        }
    }

    /// Pairs are synchronized one after another: the total time left is
    /// the remaining bytes of all of them at the combined speed.
    fn sum<'a>(pairs: impl Iterator<Item = &'a PairProgress>, now: Instant) -> Self {
        let mut total = Self::default();
        let mut elapsed = Duration::ZERO;
        for pair in pairs {
            let plan = &mut total.plan;
            plan.upload_files += pair.plan.upload_files;
            plan.upload_bytes += pair.plan.upload_bytes;
            plan.download_files += pair.plan.download_files;
            plan.download_bytes += pair.plan.download_bytes;
            plan.removals += pair.plan.removals;
            plan.unknown_size += pair.plan.unknown_size;
            total.done_files += pair.done_files;
            total.done_bytes += pair.transferred();
            total.speed += pair.speed;
            elapsed = elapsed.max(now.duration_since(pair.started));
        }
        total.eta = eta(
            total.plan.bytes().saturating_sub(total.done_bytes),
            total.speed,
            elapsed,
        );
        total
    }

    fn json(&self) -> Value {
        json!({
            "files": self.plan.files(),
            "doneFiles": self.done_files,
            "bytes": self.plan.bytes(),
            "doneBytes": self.done_bytes,
            "uploadFiles": self.plan.upload_files,
            "uploadBytes": self.plan.upload_bytes,
            "downloadFiles": self.plan.download_files,
            "downloadBytes": self.plan.download_bytes,
            "unknownSize": self.plan.unknown_size,
            "speed": self.speed.round() as u64,
            "eta": self.eta,
            "fraction": if self.plan.bytes() > 0 {
                self.done_bytes as f64 / self.plan.bytes() as f64
            } else {
                self.done_files as f64 / self.plan.files().max(1) as f64
            },
            "text": self.text(),
        })
    }

    /// "↑ 3.2 GB of 120 GB · 412 of 9,800 files · 11.4 MB/s · about 2 h 50 min left"
    pub fn text(&self) -> String {
        let arrow = match (self.plan.upload_files > 0, self.plan.download_files > 0) {
            (true, false) => "↑ ",
            (false, true) => "↓ ",
            _ => "⇅ ",
        };
        let mut parts = Vec::new();
        if self.plan.bytes() > 0 {
            parts.push(format!(
                "{arrow}{}",
                tr::tr!(
                    "{} of {}",
                    format_bytes(self.done_bytes),
                    format_bytes(self.plan.bytes())
                )
            ));
        }
        let files = tr::tr!(
            "{} of {} files",
            format_count(self.done_files),
            format_count(self.plan.files())
        );
        parts.push(if parts.is_empty() {
            format!("{arrow}{files}")
        } else {
            files
        });
        if self.speed >= 1.0 {
            parts.push(format!("{}/s", format_bytes(self.speed as u64)));
        }
        if let Some(eta) = self.eta.filter(|eta| *eta > 0) {
            parts.push(tr::tr!("about {} left", format_duration(eta)));
        }
        parts.join(" · ")
    }
}

/// Decimal units, as file managers and cloud providers show sizes.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

pub fn format_duration(secs: u64) -> String {
    match secs {
        0..60 => tr::tr!("{} s", secs),
        60..3600 => tr::tr!("{} min", secs.div_ceil(60)),
        3600..86400 => {
            let minutes = secs.div_ceil(60);
            tr::tr!("{} h {} min", minutes / 60, minutes % 60)
        }
        _ => {
            let hours = secs.div_ceil(3600);
            tr::tr!("{} d {} h", hours / 24, hours % 24)
        }
    }
}

/// Read the stats of active pairs about once a second and keep the tray's
/// progress line current. Runs until quit.
pub fn spawn_meter(shared: Shared) {
    let spawned = thread::Builder::new()
        .name("celestial-meter".into())
        .spawn(move || {
            let mut shown: Option<String> = None;
            let mut shown_at: Option<Instant> = None;
            while !shared.quit_requested() {
                thread::sleep(METER_INTERVAL);
                shared
                    .progress
                    .sample_all(|group| rclone::group_stats(group).ok());
                let summary = shared.progress.summary();
                let due =
                    summary.is_none() || shown_at.is_none_or(|at| at.elapsed() >= TRAY_INTERVAL);
                if summary != shown && due {
                    tracing::debug!(progress = ?summary, "transfer progress");
                    (shared.tray_progress)(summary.clone());
                    shown = summary;
                    shown_at = Some(Instant::now());
                }
            }
        });
    if let Err(err) = spawned {
        tracing::warn!("could not start the progress meter: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(upload_bytes: u64, files: u64) -> Plan {
        Plan {
            upload_files: files,
            upload_bytes,
            ..Plan::default()
        }
    }

    #[test]
    fn speed_follows_rclone_bytes_over_wall_clock_time() {
        let start = Instant::now();
        let mut pair = PairProgress::new("r", "/l", "p", plan(10_000, 2), start);
        pair.sample(
            start,
            GroupStats {
                bytes: 0,
                in_flight: 0,
            },
        );
        assert_eq!(pair.speed, 0.0);
        pair.sample(
            start + Duration::from_secs(2),
            GroupStats {
                bytes: 2000,
                in_flight: 2000,
            },
        );
        assert_eq!(pair.speed, 1000.0);
        assert_eq!(pair.transferred(), 2000);
        // A slower second: the speed moves towards it, not all the way.
        pair.sample(
            start + Duration::from_secs(3),
            GroupStats {
                bytes: 2100,
                in_flight: 2100,
            },
        );
        assert!(pair.speed < 1000.0 && pair.speed > 100.0, "{}", pair.speed);
        // A group that restarted is not a negative speed.
        let before = pair.speed;
        pair.sample(
            start + Duration::from_secs(4),
            GroupStats {
                bytes: 10,
                in_flight: 10,
            },
        );
        assert_eq!(pair.speed, before);
    }

    #[test]
    fn time_left_waits_for_a_measured_speed() {
        let start = Instant::now();
        let mut pair = PairProgress::new("r", "/l", "p", plan(10_000, 2), start);
        assert_eq!(pair.eta_secs(start + Duration::from_secs(10)), None);
        pair.speed = 1000.0;
        assert_eq!(pair.eta_secs(start + Duration::from_secs(1)), None);
        assert_eq!(pair.eta_secs(start + Duration::from_secs(5)), Some(10));
        pair.done_bytes = 10_000;
        assert_eq!(pair.eta_secs(start + Duration::from_secs(5)), Some(0));
    }

    #[test]
    fn finished_items_and_bytes_in_flight_never_exceed_the_plan() {
        let bus = ProgressBus::default();
        bus.start(1, "r", "/l", "p", plan(1000, 2));
        bus.sample_all(|_| {
            Some(GroupStats {
                bytes: 900,
                in_flight: 900,
            })
        });
        assert_eq!(bus.get(1).unwrap().transferred(), 900);
        bus.finished(1, 1, 600);
        bus.finished(1, 1, 400);
        bus.finished(1, 1, 400);
        let pair = bus.get(1).unwrap();
        assert_eq!((pair.done_files, pair.transferred()), (2, 1000));
        bus.end(1);
        assert!(bus.get(1).is_none());
        assert_eq!(bus.summary(), None);
    }

    #[test]
    fn a_plan_without_transfers_shows_no_progress() {
        let bus = ProgressBus::default();
        bus.start(
            1,
            "r",
            "/l",
            "p",
            Plan {
                removals: 3,
                ..Plan::default()
            },
        );
        assert!(bus.get(1).is_none());
        assert!(!bus.sample_all(|_| panic!("no pair to read")));
    }

    #[test]
    fn totals_add_up_pairs_and_describe_them() {
        let bus = ProgressBus::default();
        bus.start(1, "r", "/a", "a", plan(3_000_000_000, 10));
        bus.start(
            2,
            "r",
            "/b",
            "b",
            Plan {
                download_files: 2,
                download_bytes: 500,
                unknown_size: 1,
                ..Plan::default()
            },
        );
        bus.finished(1, 1, 1_000_000_000);
        let value: Value = serde_json::from_str(&bus.json()).unwrap();
        assert_eq!(value["pairs"].as_array().unwrap().len(), 2);
        assert_eq!(value["total"]["files"], 12);
        assert_eq!(value["total"]["doneFiles"], 1);
        assert_eq!(value["total"]["bytes"], 3_000_000_500u64);
        assert_eq!(value["total"]["unknownSize"], 1);
        assert_eq!(bus.summary().unwrap(), "⇅ 1.0 GB of 3.0 GB · 1 of 12 files");
    }

    #[test]
    fn sizes_counts_and_durations_read_naturally() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_500), "1.5 KB");
        assert_eq!(format_bytes(11_400_000), "11 MB");
        assert_eq!(format_bytes(120_000_000_000), "120 GB");
        assert_eq!(format_count(9_800), "9,800");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_duration(42), "42 s");
        assert_eq!(format_duration(61), "2 min");
        assert_eq!(format_duration(2 * 3600 + 50 * 60), "2 h 50 min");
        assert_eq!(format_duration(3 * 86400 + 3600), "3 d 1 h");
    }
}
