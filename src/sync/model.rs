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
}

impl DirState {
    pub fn awaiting() -> Self {
        Self {
            status: tr::tr!("Awaiting sync check..."),
            phase: "loading".into(),
            errors: Vec::new(),
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
                            "errorStatus": dir.error_status(),
                            "errors": dir.errors.iter().map(error_json).collect::<Vec<_>>(),
                            "exclusions": read_exclusions(local),
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

fn error_json(err: &StoredError) -> Value {
    match &err.error {
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
                "title": util::fmt_home(path),
                "detail": detail,
            })
        }
    }
}

pub fn read_exclusions(local_path: &str) -> Vec<String> {
    let path = Path::new(local_path).join(FILE_IGNORE_NAME);
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn write_exclusions(local_path: &str, text: &str) -> Result<(), String> {
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        glob::Pattern::new(line).map_err(|err| format!("{line}: {err}"))?;
    }
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

/// Snapshot slot the window reads. The worker bumps `generation` after each write.
#[derive(Clone)]
pub struct SnapshotBus {
    pub json: Arc<Mutex<String>>,
    pub generation: Arc<AtomicU64>,
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
        }
    }

    pub fn publish(&self, model: &Model) {
        let text = model.snapshot().to_string();
        *self.json.lock().expect("snapshot") = text;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    pub fn load(&self) -> (u64, String) {
        let generation = self.generation.load(Ordering::Relaxed);
        let text = self.json.lock().expect("snapshot").clone();
        (generation, text)
    }
}
