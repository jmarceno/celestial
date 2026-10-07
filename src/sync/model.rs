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
    General(String, String),
    BothMoreCurrent(String, String),
}

/// Kind (`true` for a folder) and modification time of one side of a conflict,
/// or `None` when that side does not exist.
pub type ItemVersion = Option<(bool, i64)>;

#[derive(Clone, Debug)]
pub struct StoredError {
    pub id: u64,
    pub error: SyncError,
}

#[derive(Clone, Debug)]
pub struct DirState {
    pub status: String,
    pub phase: String,
    pub error_status: String,
    pub errors: Vec<StoredError>,
}

impl DirState {
    pub fn awaiting() -> Self {
        Self {
            status: tr::tr!("Awaiting sync check..."),
            phase: "loading".into(),
            error_status: String::new(),
            errors: Vec::new(),
        }
    }

    fn error_count(&self) -> i32 {
        self.error_status
            .split_whitespace()
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0)
    }

    fn set_error_count(&mut self, count: i32) {
        self.error_status = if count <= 0 {
            String::new()
        } else if count == 1 {
            tr::tr!("1 error found.") + " "
        } else {
            tr::tr!("{} errors found.", count) + " "
        };
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
    /// The (local, remote) versions shown in an open conflict prompt, by error ID.
    pub conflict_versions: HashMap<u64, (ItemVersion, ItemVersion)>,
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
            conflict_versions: HashMap::new(),
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

    pub fn has_errors(&self, remote: &str, local: &str, remote_path: &str) -> bool {
        self.remotes
            .get(remote)
            .and_then(|dirs| dirs.get(&(local.to_owned(), remote_path.to_owned())))
            .is_some_and(|dir| !dir.error_status.is_empty())
    }

    pub fn error_dir_count(&self) -> i32 {
        self.remotes
            .values()
            .flat_map(|dirs| dirs.values())
            .filter(|dir| !dir.error_status.is_empty())
            .count() as i32
    }

    pub fn add_error(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        error: SyncError,
    ) -> bool {
        let id = self.next_error_id;
        self.next_error_id += 1;
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        let count = dir.error_count() + 1;
        dir.set_error_count(count);
        dir.errors.push(StoredError { id, error });
        true
    }

    pub fn dismiss_error(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        id: u64,
        please_resolve: &str,
    ) -> bool {
        self.conflict_versions.remove(&id);
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        let before = dir.errors.len();
        dir.errors.retain(|err| err.id != id);
        if dir.errors.len() == before {
            return false;
        }
        let count = dir.error_count().saturating_sub(1);
        dir.set_error_count(count);
        if count == 0 {
            if let Some(text) = dir.status.strip_suffix(please_resolve) {
                dir.status = text.to_owned();
            }
            if dir.phase == "warning" {
                dir.phase = "done".into();
            }
        }
        true
    }

    pub fn error_by_id(
        &self,
        remote: &str,
        local: &str,
        remote_path: &str,
        id: u64,
    ) -> Option<SyncError> {
        self.remotes
            .get(remote)
            .and_then(|dirs| dirs.get(&(local.to_owned(), remote_path.to_owned())))
            .and_then(|dir| dir.errors.iter().find(|err| err.id == id))
            .map(|err| err.error.clone())
    }

    pub fn mark_finished(
        &mut self,
        remote: &str,
        local: &str,
        remote_path: &str,
        please_resolve: &str,
    ) -> bool {
        let Some(dir) = self.dir_mut(remote, local, remote_path) else {
            return false;
        };
        if dir.error_status.is_empty() {
            dir.status = tr::tr!("Files are synced.");
            dir.phase = "done".into();
        } else {
            let mut text = tr::tr!("Files are synced.");
            text.push_str(please_resolve);
            dir.status = text;
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
                            "errorStatus": dir.error_status.trim(),
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
        SyncError::BothMoreCurrent(local, remote) => json!({
            "id": err.id,
            "kind": "conflict",
            "title": tr::tr!(
                "Both '{}' and '{}' are more recent than at last sync.",
                local,
                remote
            ),
            "detail": "",
        }),
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
