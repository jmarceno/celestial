//! In-memory view of remotes, directories, and sync errors.
//!
//! The sync worker is the only writer. The window reads a JSON snapshot.

use crate::util;
use indexmap::IndexMap;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const FILE_IGNORE_NAME: &str = ".sync-exclude.lst";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncError {
    /// A problem with the whole pair (unavailable root, unreadable rules,
    /// database failure). The pair pauses until a later check succeeds.
    General(String, String),
    /// One item that is retried automatically or blocked until changed.
    Item {
        rel: String,
        path: String,
        state: String,
        reason: String,
        next_attempt: Option<i64>,
    },
    /// A conflict that was resolved automatically, kept until acknowledged.
    Conflict {
        id: i64,
        rel: String,
        kind: String,
        /// JSON describing both versions and where each one now lives.
        detail: String,
    },
    /// The remote has less free space than the pair would upload. The pair
    /// waits until there is space or the person approves the upload.
    Space { needed: u64, free: u64 },
}

/// Key of the pair-level error; item issues are keyed by relative path.
pub const PAIR_KEY: &str = "\0pair";

#[derive(Clone, Debug)]
pub struct StoredError {
    pub id: u64,
    pub key: String,
    pub error: SyncError,
}

#[derive(Clone, Debug)]
pub struct DirState {
    pub status: String,
    pub phase: String,
    pub errors: Vec<StoredError>,
    /// Preserved versions that can be restored, newest first.
    pub recovery: Vec<Value>,
    /// Recently synchronized files, newest first.
    pub recent: Vec<Value>,
    /// Waiting for the person to start it; no pass runs.
    pub paused: bool,
    /// How many days preserved versions are kept; 0 keeps them for good.
    pub keep_days: i64,
    /// The largest file the pair keeps a preserved copy of; 0 keeps none.
    pub recovery_max_bytes: i64,
    /// How long the pair waits between passes; 0 synchronizes as changes are
    /// detected.
    pub sync_interval_secs: i64,
}

impl DirState {
    pub fn awaiting() -> Self {
        Self {
            status: tr::tr!("Awaiting sync check..."),
            phase: "loading".into(),
            errors: Vec::new(),
            recovery: Vec::new(),
            recent: Vec::new(),
            paused: false,
            keep_days: 30,
            recovery_max_bytes: super::store::DEFAULT_RECOVERY_MAX_BYTES,
            sync_interval_secs: 0,
        }
    }

    fn error_status(&self) -> String {
        match self.errors.len() {
            0 => String::new(),
            1 => tr::tr!("1 error found."),
            count => tr::tr!("{} errors found.", count),
        }
    }

    pub fn has_pair_error(&self) -> bool {
        self.errors.iter().any(|err| err.key == PAIR_KEY)
    }
}

#[derive(Clone, Debug)]
pub struct Model {
    pub remotes: IndexMap<String, IndexMap<(String, String), DirState>>,
    pub tray_message: String,
    pub tray_phase: String,
    next_error_id: u64,
    pub remote_deletions: Vec<String>,
    pub dir_deletions: Vec<(String, String, String)>,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    pub fn new() -> Self {
        Self {
            remotes: IndexMap::new(),
            tray_message: tr::tr!("Awaiting sync checks..."),
            tray_phase: "loading".into(),
            next_error_id: 1,
            remote_deletions: Vec::new(),
            dir_deletions: Vec::new(),
        }
    }

    pub fn ensure_remote(&mut self, name: &str) {
        self.remotes.entry(name.to_owned()).or_default();
    }

    pub fn ensure_dir(&mut self, remote: &str, local: &str, remote_path: &str) {
        let dirs = self.remotes.entry(remote.to_owned()).or_default();
        dirs.entry((local.to_owned(), remote_path.to_owned()))
            .or_insert_with(DirState::awaiting);
    }

    pub fn remove_remote(&mut self, name: &str) {
        self.remotes.shift_remove(name);
    }

    /// Rename a server, keeping its folders and its place in the list.
    /// Nothing is done when the name is unknown or already taken.
    pub fn rename_remote(&mut self, from: &str, to: &str) {
        if from == to || !self.remotes.contains_key(from) || self.remotes.contains_key(to) {
            return;
        }
        let listed = std::mem::take(&mut self.remotes);
        for (name, dirs) in listed {
            let name = if name == from { to.to_owned() } else { name };
            self.remotes.insert(name, dirs);
        }
    }

    pub fn remove_dir(&mut self, remote: &str, local: &str, remote_path: &str) {
        if let Some(dirs) = self.remotes.get_mut(remote) {
            dirs.shift_remove(&(local.to_owned(), remote_path.to_owned()));
        }
    }

    fn dir_mut(&mut self, remote: &str, local: &str, remote_path: &str) -> Option<&mut DirState> {
        self.remotes
            .get_mut(remote)
            .and_then(|dirs| dirs.get_mut(&(local.to_owned(), remote_path.to_owned())))
    }

    pub fn dir(&self, remote: &str, local: &str, remote_path: &str) -> Option<&DirState> {
        self.remotes
            .get(remote)
            .and_then(|dirs| dirs.get(&(local.to_owned(), remote_path.to_owned())))
    }

    pub fn set_status(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        status: String,
    ) -> bool {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        dir.status = status;
        true
    }

    pub fn set_phase(&mut self, remote: &str, local: &str, remote_path: &str, phase: &str) -> bool {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        dir.phase = phase.to_owned();
        true
    }

    /// A paused pair waits for the person; resuming it awaits its first check.
    pub fn set_paused(&mut self, remote: &str, local: &str, remote_path: &str, paused: bool) {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return;
        };
        if dir.paused == paused {
            return;
        }
        dir.paused = paused;
        if paused {
            dir.status = tr::tr!("Paused. Review the exclusions, then start syncing.");
            dir.phase = "paused".into();
        } else {
            dir.status = tr::tr!("Awaiting sync check...");
            dir.phase = "loading".into();
        }
    }

    /// Whether the pair is paused by a pair-level problem.
    pub fn has_errors(&self, remote: &str, local: &str, remote_path: &str) -> bool {
        self.dir(remote, local, remote_path)
            .is_some_and(DirState::has_pair_error)
    }

    pub fn error_dir_count(&self) -> i32 {
        self.remotes
            .values()
            .flat_map(|dirs| dirs.values())
            .filter(|dir| !dir.errors.is_empty())
            .count() as i32
    }

    /// Record or clear the pair-level problem.
    pub fn set_pair_error(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        error: Option<SyncError>,
    ) -> bool {
        let mut next = self.next_error_id;
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        let existing = dir.errors.iter().position(|err| err.key == PAIR_KEY);
        match (existing, error) {
            (Some(index), Some(error)) => dir.errors[index].error = error,
            (Some(index), None) => {
                dir.errors.remove(index);
            }
            (None, Some(error)) => {
                dir.errors.insert(
                    0,
                    StoredError {
                        id: next,
                        key: PAIR_KEY.into(),
                        error,
                    },
                );
                next += 1;
            }
            (None, None) => {}
        }
        self.next_error_id = next;
        true
    }

    /// Replace the item issues of a pair, keeping the IDs of issues that are
    /// still present so an open prompt keeps referring to the same item.
    pub fn set_issues(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        issues: Vec<(String, SyncError)>,
    ) -> bool {
        let mut next = self.next_error_id;
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        let old: HashMap<String, u64> = dir
            .errors
            .iter()
            .map(|err| (err.key.clone(), err.id))
            .collect();
        dir.errors.retain(|err| err.key == PAIR_KEY);
        for (key, error) in issues {
            let id = old.get(&key).copied().unwrap_or_else(|| {
                next += 1;
                next - 1
            });
            dir.errors.push(StoredError { id, key, error });
        }
        self.next_error_id = next;
        true
    }

    pub fn set_recovery(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        items: Vec<Value>,
    ) -> bool {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        dir.recovery = items;
        true
    }

    pub fn set_keep_days(&mut self, remote: &str, local: &str, remote_path: &str, days: i64) {
        if let Some(dir) = self.dir_mut(remote, local, remote_path) {
            dir.keep_days = days;
        }
    }

    pub fn set_recovery_max_bytes(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        bytes: i64,
    ) {
        if let Some(dir) = self.dir_mut(remote, local, remote_path) {
            dir.recovery_max_bytes = bytes;
        }
    }

    pub fn set_sync_interval_secs(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        secs: i64,
    ) {
        if let Some(dir) = self.dir_mut(remote, local, remote_path) {
            dir.sync_interval_secs = secs;
        }
    }

    pub fn set_recent(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        items: Vec<Value>,
    ) -> bool {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        dir.recent = items;
        true
    }

    /// Put a just synchronized file at the top of the recent list.
    pub fn push_recent(&mut self, remote: &str, local: &str, remote_path: &str, item: Value) {
        if let Some(dir) = self.dir_mut(remote, local, remote_path) {
            dir.recent.insert(0, item);
            dir.recent.truncate(RECENT_SHOWN);
        }
    }

    /// Record a pair-level problem (kept for callers that only add one).
    pub fn add_error(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        error: SyncError,
    ) -> bool {
        self.set_pair_error(remote, local, remote_path, Some(error))
    }

    /// Remove an issue from the view. The caller decides what dismissal
    /// means for the item (retry now, acknowledge).
    pub fn dismiss_error(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        id: u64,
    ) -> Option<StoredError> {
        let dir = self.dir_mut(remote, local, remote_path)?;
        let index = dir.errors.iter().position(|err| err.id == id)?;
        let removed = dir.errors.remove(index);
        if dir.errors.is_empty() && dir.phase == "warning" {
            dir.phase = "done".into();
        }
        Some(removed)
    }

    pub fn error_by_id(
        &self,
        remote: &str,
        local: &str,
        remote_path: &str,
        id: u64,
    ) -> Option<StoredError> {
        self.dir(remote, local, remote_path)
            .and_then(|dir| dir.errors.iter().find(|err| err.id == id))
            .cloned()
    }

    pub fn mark_finished(&mut self, remote: &str, local: &str, remote_path: &str) -> bool {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        if dir.errors.is_empty() {
            dir.status = tr::tr!("Files are synced.");
            dir.phase = "done".into();
        } else {
            dir.status = tr::tr!("Files are synced. Some items need attention.");
            dir.phase = "warning".into();
        }
        true
    }

    pub fn snapshot(&self) -> Value {
        let remotes: Vec<Value> = self
            .remotes
            .iter()
            .map(|(name, dirs)| {
                let dir_json: Vec<Value> = dirs
                    .iter()
                    .map(|((local, remote), dir)| {
                        json!({
                            "localPath": local,
                            "localDisplay": util::fmt_home(local),
                            "remotePath": remote,
                            "remoteDisplay": format!("/{remote}"),
                            "status": dir.status,
                            "phase": dir.phase,
                            "paused": dir.paused,
                            "errorStatus": dir.error_status(),
                            "errors": dir.errors.iter().map(error_json).collect::<Vec<_>>(),
                            "exclusions": read_exclusions(local),
                            "recovery": dir.recovery,
                            "keepDays": dir.keep_days,
                            "recoveryMaxBytes": dir.recovery_max_bytes,
                            "syncIntervalSecs": dir.sync_interval_secs,
                            "recent": dir.recent,
                        })
                    })
                    .collect();
                json!({ "name": name, "dirs": dir_json })
            })
            .collect();
        json!({
            "trayMessage": self.tray_message,
            "trayPhase": self.tray_phase,
            "remotes": remotes,
        })
    }
}

/// Recent changes shown per pair.
pub const RECENT_SHOWN: usize = 50;

/// A recent change as the window shows it.
pub fn activity_json(local_root: &str, activity: &super::store::Activity) -> Value {
    let path = Path::new(local_root).join(&activity.rel);
    let name = activity
        .rel
        .rsplit('/')
        .next()
        .unwrap_or(&activity.rel)
        .to_owned();
    let label = match activity.action.as_str() {
        "uploaded" => tr::tr!("Uploaded"),
        "downloaded" => tr::tr!("Downloaded"),
        "deleted_local" => tr::tr!("Deleted from this computer"),
        "deleted_remote" => tr::tr!("Deleted from the remote"),
        "conflict" => tr::tr!("Both versions kept"),
        other => other.to_owned(),
    };
    json!({
        "rel": activity.rel,
        "name": name,
        "path": path.display().to_string(),
        "folder": path.parent().map(|parent| parent.display().to_string()).unwrap_or_default(),
        "action": activity.action,
        "label": label,
        "size": activity.size,
        "sizeText": if activity.size >= 0 {
            super::progress::format_bytes(activity.size.unsigned_abs())
        } else {
            String::new()
        },
        "at": activity.finished_at,
    })
}

fn error_json(err: &StoredError) -> Value {
    match &err.error {
        SyncError::Space { needed, free } => json!({
            "id": err.id,
            "kind": "space",
            "title": tr::tr!("Not enough space on the remote"),
            "detail": space_text(*needed, *free),
        }),
        SyncError::Conflict {
            rel, kind, detail, ..
        } => json!({
            "id": err.id,
            "kind": "conflict",
            "title": conflict_title(kind, rel),
            "detail": conflict_summary(kind, detail),
        }),
        SyncError::General(path, message) => json!({
            "id": err.id,
            "kind": "general",
            "title": path,
            "detail": message,
        }),
        SyncError::Item {
            path,
            state,
            reason,
            next_attempt,
            ..
        } => {
            let detail = match next_attempt {
                Some(at) if state == "retrying" => {
                    let when = time::OffsetDateTime::from_unix_timestamp(*at)
                        .ok()
                        .map(|time| format!("{:02}:{:02} UTC", time.hour(), time.minute()))
                        .unwrap_or_default();
                    format!(
                        "{reason}\n{}",
                        tr::tr!("Retrying automatically at {}.", when)
                    )
                }
                _ => reason.clone(),
            };
            json!({
                "id": err.id,
                "kind": state,
                "warning": super::observe::is_limitation(reason),
                "title": util::fmt_home(path),
                "detail": detail,
            })
        }
    }
}

pub fn space_text(needed: u64, free: u64) -> String {
    tr::tr!(
        "This folder needs {} more on the remote, but only {} is free. Nothing was uploaded. Free some space, or sync anyway to upload until the remote is full.",
        super::progress::format_bytes(needed),
        super::progress::format_bytes(free)
    )
}

pub fn conflict_title(kind: &str, rel: &str) -> String {
    match kind {
        "edit_delete" => tr::tr!("'{}' was deleted on one side and changed on the other", rel),
        "file_folder" => tr::tr!("A file and a folder were both named '{}'", rel),
        "bootstrap" => tr::tr!("The local and remote contents of '{}' differ", rel),
        _ => tr::tr!("Both versions of '{}' were kept", rel),
    }
}

/// Where each version of a resolved conflict now lives.
pub fn conflict_summary(kind: &str, detail: &str) -> String {
    let detail: Value = serde_json::from_str(detail).unwrap_or(Value::Null);
    let text = |key: &str| detail[key].as_str().unwrap_or_default().to_owned();
    let version = |side: &str| {
        let value = &detail[side];
        if value.is_null() {
            return String::new();
        }
        if value["isDir"].as_bool() == Some(true) {
            return tr::tr!("a folder");
        }
        tr::tr!(
            "{} bytes, modified {}",
            value["size"].as_i64().unwrap_or(-1),
            value["mtime"].as_str().unwrap_or("?")
        )
    };
    if kind == "edit_delete" {
        let restored_on = text("restoredOn");
        let changed = if restored_on == "local" {
            "remote"
        } else {
            "local"
        };
        return tr::tr!(
            "The changed {} version ({}) was restored on the {} side. Delete it again if it is no longer needed.",
            changed,
            version(changed),
            restored_on
        );
    }
    let copy_side = text("copySide");
    let other = if copy_side == "local" {
        "remote"
    } else {
        "local"
    };
    tr::tr!(
        "The {} version ({}) is now '{}'. The {} version ({}) keeps the name. Both are on both sides.",
        copy_side,
        version(&copy_side),
        text("copy"),
        other,
        version(other)
    )
}

/// Identity of a rules file: it is read again only when this changes.
type RulesIdentity = (u64, u64, i64, i64, u64);

type RulesCache = HashMap<String, (RulesIdentity, Vec<String>)>;

static EXCLUSIONS: Mutex<Option<RulesCache>> = Mutex::new(None);

pub fn read_exclusions(local_path: &str) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let path = Path::new(local_path).join(FILE_IGNORE_NAME);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return super::exclusions::defaults();
        }
        Err(_) => return Vec::new(),
    };
    let identity = (
        metadata.dev(),
        metadata.ino(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.size(),
    );
    let mut cache = EXCLUSIONS.lock().unwrap_or_else(|err| err.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some((known, lines)) = cache.get(local_path) {
        if *known == identity {
            return lines.clone();
        }
    }
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let lines: Vec<String> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    cache.insert(local_path.to_owned(), (identity, lines.clone()));
    lines
}

pub fn write_exclusions(local_path: &str, text: &str) -> Result<(), String> {
    super::exclusions::validate(text)?;
    let path = Path::new(local_path).join(FILE_IGNORE_NAME);
    if !Path::new(local_path).is_dir() {
        return Err(tr::tr!("The specified local directory doesn't exist"));
    }
    let body: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    let mut lock = file_lock::FileLock::lock(
        &path,
        true,
        file_lock::FileOptions::new()
            .create(true)
            .read(true)
            .write(true),
    )
    .map_err(|err| err.to_string())?;
    use std::io::{Seek, Write};
    lock.file.set_len(0).map_err(|err| err.to_string())?;
    lock.file.rewind().map_err(|err| err.to_string())?;
    lock.file
        .write_all(body.join("\n").as_bytes())
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// Snapshot slot the window reads. The worker bumps `generation` whenever
/// the snapshot changes.
#[derive(Clone)]
pub struct SnapshotBus {
    pub json: Arc<Mutex<String>>,
    pub generation: Arc<AtomicU64>,
    last_tray: Arc<Mutex<Option<(String, String)>>>,
    /// Tray updates sent, for measurement.
    pub tray_updates: Arc<AtomicU64>,
}

impl Default for SnapshotBus {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotBus {
    pub fn new() -> Self {
        Self {
            json: Arc::new(Mutex::new("{}".into())),
            generation: Arc::new(AtomicU64::new(0)),
            last_tray: Arc::new(Mutex::new(None)),
            tray_updates: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn publish(&self, model: &Model) {
        let text = model.snapshot().to_string();
        let mut json = self.json.lock().unwrap_or_else(|err| err.into_inner());
        if *json != text {
            *json = text;
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Whether the tray shows something else than `message` and `phase`;
    /// records them as shown when it does.
    pub fn tray_changed(&self, message: &str, phase: &str) -> bool {
        let mut last = self.last_tray.lock().unwrap_or_else(|err| err.into_inner());
        if last
            .as_ref()
            .is_some_and(|(shown, shown_phase)| shown == message && shown_phase == phase)
        {
            return false;
        }
        *last = Some((message.to_owned(), phase.to_owned()));
        self.tray_updates.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub fn load(&self) -> (u64, String) {
        let generation = self.generation.load(Ordering::Relaxed);
        let text = self
            .json
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        (generation, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_snapshots_and_tray_states_are_not_republished() {
        let bus = SnapshotBus::new();
        let model = Model::new();
        bus.publish(&model);
        bus.publish(&model);
        assert_eq!(bus.generation.load(Ordering::Relaxed), 1);
        assert!(bus.tray_changed("a", "done"));
        assert!(!bus.tray_changed("a", "done"));
        assert!(bus.tray_changed("a", "syncing"));
        assert_eq!(bus.tray_updates.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn renaming_a_server_keeps_its_place_and_its_folders() {
        let mut model = Model::new();
        model.ensure_dir("beta", "/one", "a");
        model.ensure_dir("alpha", "/two", "b");
        model.set_status("beta", "/one", "a", tr::tr!("Files are synced."));
        model.rename_remote("beta", "gamma");
        let names: Vec<&str> = model.remotes.keys().map(String::as_str).collect();
        // The renamed server keeps the slot it had in the list.
        assert_eq!(names, vec!["gamma", "alpha"]);
        assert_eq!(
            model.dir("gamma", "/one", "a").unwrap().status,
            tr::tr!("Files are synced.")
        );
        assert!(model.dir("beta", "/one", "a").is_none());
        // A name that is not there changes nothing.
        model.rename_remote("nope", "other");
        assert_eq!(model.remotes.len(), 2);
        // Neither does a name the list already uses.
        model.rename_remote("alpha", "gamma");
        assert_eq!(
            model.remotes.keys().collect::<Vec<&String>>(),
            vec!["gamma", "alpha"]
        );
    }

    #[test]
    fn exclusions_are_reread_only_when_the_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_str().unwrap();
        fs::write(temp.path().join(FILE_IGNORE_NAME), "*.tmp\n").unwrap();
        assert_eq!(read_exclusions(root), vec!["*.tmp"]);
        fs::write(temp.path().join(FILE_IGNORE_NAME), "*.tmp\n*.bak\n").unwrap();
        assert_eq!(read_exclusions(root), vec!["*.tmp", "*.bak"]);
    }
}
