//! Local change hints from inotify, through the `notify` crate.
//!
//! Events never decide what to synchronize. They only tell the worker that a
//! pair's local tree may have changed, so an idle pair can skip the full local
//! scan. Whenever the hints could be incomplete (no watcher, an overflowed
//! event queue, the watch limit, a network or FUSE filesystem, a replaced or
//! remounted root) the pair is scanned as before, and every watched pair is
//! still scanned completely at [`FULL_SCAN_INTERVAL`].

use notify::{
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{AccessKind, AccessMode, ModifyKind},
};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often a watched pair is still scanned completely.
const FULL_SCAN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How long to wait before retrying a watch that failed or hit the limit.
const RETRY_INTERVAL: Duration = Duration::from_secs(10 * 60);

struct PairState {
    root: PathBuf,
    /// An event arrived since the last check.
    dirty: bool,
    /// When a full scan last found the local tree matching the history.
    verified_at: Option<Instant>,
}

#[derive(Default)]
struct Hints {
    pairs: HashMap<i32, PairState>,
    /// The watcher reported an error; its hints can no longer be trusted.
    broken: bool,
    /// Roots that were removed or moved; the kernel dropped their watches.
    lost_roots: HashSet<PathBuf>,
}

impl Hints {
    fn mark_all(&mut self) {
        for pair in self.pairs.values_mut() {
            pair.dirty = true;
        }
    }
}

pub struct LocalWatcher {
    watcher: Option<RecommendedWatcher>,
    hints: Arc<Mutex<Hints>>,
    /// Watched roots and the (device, inode) they had when the watch began.
    watches: HashMap<PathBuf, (u64, u64)>,
    retry_after: HashMap<PathBuf, Instant>,
    broken_since: Option<Instant>,
}

impl Default for LocalWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalWatcher {
    pub fn new() -> Self {
        let hints = Arc::new(Mutex::new(Hints::default()));
        let watcher = new_watcher(&hints);
        if watcher.is_none() {
            lock(&hints).broken = true;
        }
        Self {
            watcher,
            hints,
            watches: HashMap::new(),
            retry_after: HashMap::new(),
            broken_since: None,
        }
    }

    /// Track exactly these (pair ID, local root) pairs and watch their roots.
    pub fn sync_pairs(&mut self, pairs: &[(i32, PathBuf)]) {
        let now = Instant::now();
        self.recover(now);
        self.drop_lost_roots();
        {
            let mut hints = lock(&self.hints);
            let ids: HashSet<i32> = pairs.iter().map(|(id, _)| *id).collect();
            hints.pairs.retain(|id, _| ids.contains(id));
            for (id, root) in pairs {
                let pair = hints.pairs.entry(*id).or_insert_with(|| PairState {
                    root: root.clone(),
                    dirty: true,
                    verified_at: None,
                });
                if pair.root != *root {
                    pair.root = root.clone();
                    pair.verified_at = None;
                }
            }
        }
        let wanted: HashSet<&PathBuf> = pairs.iter().map(|(_, root)| root).collect();
        let unwanted: Vec<PathBuf> = self
            .watches
            .keys()
            .filter(|root| !wanted.contains(root))
            .cloned()
            .collect();
        for root in unwanted {
            self.unwatch(&root);
        }
        if lock(&self.hints).broken {
            return;
        }
        for root in wanted {
            if self.watches.contains_key(root)
                || self.retry_after.get(root).is_some_and(|until| *until > now)
            {
                continue;
            }
            self.watch(root, now);
        }
    }

    /// Whether the pair's local tree is known to match its history, so the
    /// full local scan can be skipped. Returning `false` starts a new
    /// observation: events from now on count against the next check.
    pub fn local_unchanged(&mut self, id: i32, root: &Path) -> bool {
        self.drop_lost_roots();
        let watching = match self.watches.get(root) {
            Some(identity) if identity_of(root) == Some(*identity) => true,
            Some(_) => {
                // Remounted or replaced: the old watch sees nothing.
                tracing::info!(root = %root.display(), "sync folder was replaced; watching it again");
                self.unwatch(root);
                false
            }
            None => false,
        };
        let mut hints = lock(&self.hints);
        let broken = hints.broken;
        let Some(pair) = hints.pairs.get_mut(&id) else {
            return false;
        };
        let unchanged = watching
            && !broken
            && !pair.dirty
            && pair
                .verified_at
                .is_some_and(|at| at.elapsed() < FULL_SCAN_INTERVAL);
        if !unchanged {
            pair.dirty = false;
            pair.verified_at = None;
        }
        unchanged
    }

    /// A full scan found the pair's local tree matching its history.
    pub fn mark_verified(&mut self, id: i32) {
        if let Some(pair) = lock(&self.hints).pairs.get_mut(&id) {
            pair.verified_at = Some(Instant::now());
        }
    }

    fn watch(&mut self, root: &PathBuf, now: Instant) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        if let Some(kind) = unreliable_filesystem(root) {
            tracing::info!(root = %root.display(), "{kind} filesystem; scanning instead of watching");
            self.retry_after.insert(root.clone(), now + RETRY_INTERVAL);
            return;
        }
        // A missing root is retried on the next pass; the pair reports it.
        let Some(identity) = identity_of(root) else {
            return;
        };
        match watcher.watch(root, RecursiveMode::Recursive) {
            Ok(()) => {
                self.watches.insert(root.clone(), identity);
                self.retry_after.remove(root);
                // Events before this point were missed: require a full scan.
                for pair in lock(&self.hints).pairs.values_mut() {
                    if pair.root == *root {
                        pair.dirty = true;
                        pair.verified_at = None;
                    }
                }
            }
            Err(err) => {
                let _ = watcher.unwatch(root);
                tracing::warn!(root = %root.display(), "could not watch sync folder; scanning instead: {err}");
                self.retry_after.insert(root.clone(), now + RETRY_INTERVAL);
            }
        }
    }

    fn drop_lost_roots(&mut self) {
        let lost: Vec<PathBuf> = lock(&self.hints).lost_roots.drain().collect();
        for root in lost {
            self.unwatch(&root);
        }
    }

    fn unwatch(&mut self, root: &Path) {
        self.watches.remove(root);
        if let Some(watcher) = self.watcher.as_mut() {
            let _ = watcher.unwatch(root);
        }
    }

    /// Replace a watcher that reported an error, after a delay.
    fn recover(&mut self, now: Instant) {
        if !lock(&self.hints).broken {
            self.broken_since = None;
            return;
        }
        let since = *self.broken_since.get_or_insert(now);
        if now.duration_since(since) < RETRY_INTERVAL {
            return;
        }
        self.watcher = None;
        self.watches.clear();
        self.retry_after.clear();
        self.broken_since = None;
        self.watcher = new_watcher(&self.hints);
        let mut hints = lock(&self.hints);
        hints.broken = self.watcher.is_none();
        hints.mark_all();
    }
}

fn new_watcher(hints: &Arc<Mutex<Hints>>) -> Option<RecommendedWatcher> {
    let handler_hints = Arc::clone(hints);
    match RecommendedWatcher::new(
        move |result| handle_event(&handler_hints, result),
        Config::default().with_follow_symlinks(false),
    ) {
        Ok(watcher) => Some(watcher),
        Err(err) => {
            tracing::warn!("file watcher unavailable; scanning sync folders instead: {err}");
            None
        }
    }
}

fn handle_event(hints: &Mutex<Hints>, result: notify::Result<Event>) {
    let mut hints = lock(hints);
    match result {
        // Reading and listing (including Celestial's own scans) change nothing.
        Ok(event) if is_read_only(&event.kind) => {}
        Ok(event) if !event.need_rescan() && !event.paths.is_empty() => {
            let root_lost = matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
            );
            for path in &event.paths {
                if root_lost && hints.pairs.values().any(|pair| pair.root == *path) {
                    hints.lost_roots.insert(path.clone());
                }
                let mut matched = false;
                for pair in hints.pairs.values_mut() {
                    if path.starts_with(&pair.root) {
                        pair.dirty = true;
                        matched = true;
                    }
                }
                if !matched {
                    hints.mark_all();
                }
            }
        }
        // Queue overflow or an event without a path: anything may have changed.
        Ok(_) => hints.mark_all(),
        Err(err) => {
            tracing::warn!("file watcher failed; scanning sync folders instead: {err}");
            hints.broken = true;
            hints.mark_all();
        }
    }
}

fn is_read_only(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(
            AccessKind::Open(_) | AccessKind::Read | AccessKind::Close(AccessMode::Read)
        )
    )
}

fn identity_of(root: &Path) -> Option<(u64, u64)> {
    std::fs::metadata(root)
        .ok()
        .filter(|metadata| metadata.is_dir())
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// inotify only reports changes made through this machine's kernel, so it
/// misses edits made by other machines on network and FUSE filesystems.
fn unreliable_filesystem(root: &Path) -> Option<&'static str> {
    use nix::sys::statfs::{FUSE_SUPER_MAGIC, FsType, NFS_SUPER_MAGIC, SMB_SUPER_MAGIC, statfs};
    const CIFS_MAGIC: FsType = FsType(0xFF53_4D42);
    const SMB2_MAGIC: FsType = FsType(0xFE53_4D42);
    const V9FS_MAGIC: FsType = FsType(0x0102_1997);
    const CEPH_MAGIC: FsType = FsType(0x00C3_6400);
    let kind = statfs(root).ok()?.filesystem_type();
    [
        (NFS_SUPER_MAGIC, "NFS"),
        (SMB_SUPER_MAGIC, "SMB"),
        (CIFS_MAGIC, "SMB"),
        (SMB2_MAGIC, "SMB"),
        (FUSE_SUPER_MAGIC, "FUSE"),
        (V9FS_MAGIC, "9P"),
        (CEPH_MAGIC, "Ceph"),
    ]
    .into_iter()
    .find(|(magic, _)| *magic == kind)
    .map(|(_, name)| name)
}

fn lock(hints: &Mutex<Hints>) -> MutexGuard<'_, Hints> {
    hints.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, thread};

    fn watched(root: &Path) -> LocalWatcher {
        let mut watcher = LocalWatcher::new();
        watcher.sync_pairs(&[(1, root.to_path_buf())]);
        assert!(!watcher.local_unchanged(1, root), "first check must scan");
        watcher.mark_verified(1);
        assert!(watcher.local_unchanged(1, root));
        watcher
    }

    fn wait_for_change(watcher: &mut LocalWatcher, root: &Path) -> bool {
        for _ in 0..40 {
            thread::sleep(Duration::from_millis(50));
            if !watcher.local_unchanged(1, root) {
                return true;
            }
        }
        false
    }

    #[test]
    fn edits_mark_the_pair_for_a_scan() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        let mut watcher = watched(temp.path());
        fs::write(temp.path().join("nested/file.txt"), "edit").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
        // The next check needs a new verification, not just a quiet period.
        assert!(!watcher.local_unchanged(1, temp.path()));
    }

    #[test]
    fn new_folders_are_watched() {
        let temp = tempfile::tempdir().unwrap();
        let mut watcher = watched(temp.path());
        fs::create_dir(temp.path().join("new")).unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
        thread::sleep(Duration::from_millis(200));
        watcher.local_unchanged(1, temp.path());
        watcher.mark_verified(1);
        fs::write(temp.path().join("new/file.txt"), "edit").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
    }

    #[test]
    fn reading_and_listing_do_not_mark_the_pair() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file.txt"), "data").unwrap();
        let mut watcher = watched(temp.path());
        for entry in fs::read_dir(temp.path()).unwrap() {
            fs::read(entry.unwrap().path()).unwrap();
        }
        thread::sleep(Duration::from_millis(300));
        assert!(watcher.local_unchanged(1, temp.path()));
    }

    #[test]
    fn replaced_root_requires_a_scan_and_a_new_watch() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        let mut watcher = watched(&root);
        fs::remove_dir(&root).unwrap();
        fs::create_dir(&root).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(!watcher.local_unchanged(1, &root));
        watcher.sync_pairs(&[(1, root.clone())]);
        assert!(!watcher.local_unchanged(1, &root));
        watcher.mark_verified(1);
        fs::write(root.join("file.txt"), "edit").unwrap();
        assert!(wait_for_change(&mut watcher, &root));
    }

    #[test]
    fn pairs_sharing_a_root_are_tracked_separately() {
        let temp = tempfile::tempdir().unwrap();
        let mut watcher = LocalWatcher::new();
        let root = temp.path().to_path_buf();
        watcher.sync_pairs(&[(1, root.clone()), (2, root.clone())]);
        assert!(!watcher.local_unchanged(1, &root));
        watcher.mark_verified(1);
        assert!(watcher.local_unchanged(1, &root));
        assert!(!watcher.local_unchanged(2, &root));
    }
}
