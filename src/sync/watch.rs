//! Local change hints from inotify, through the `notify` crate.
//!
//! Each pair's root and every folder its last full scan included are
//! watched one by one, so excluded folders (caches, build output) cost no
//! watches and no events. A new folder is reported by its parent; the scan
//! that follows includes it and it is watched from then on.
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
/// A burst of events (an atomic save, a build, an unpacked archive) is
/// complete once nothing arrived for this long...
pub const QUIET_WINDOW: Duration = Duration::from_millis(250);
/// ...or once it has lasted this long, so a continuous writer cannot
/// postpone synchronization forever. A file still being written is then
/// retried by the pass, never copied unstable.
pub const MAX_DELAY: Duration = Duration::from_secs(2);

struct PairState {
    root: PathBuf,
    /// Celestial's own internal folder: its events are not changes.
    internal: Option<PathBuf>,
    /// An event arrived since the last check.
    dirty: bool,
    /// The first and the latest event of the current burst.
    burst: Option<(Instant, Instant)>,
    /// When a full scan last found the local tree matching the history.
    verified_at: Option<Instant>,
}

impl PairState {
    fn touch(&mut self, now: Instant) {
        self.dirty = true;
        self.burst = Some(match self.burst {
            Some((first, _)) => (first, now),
            None => (now, now),
        });
    }
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
        let now = Instant::now();
        for pair in self.pairs.values_mut() {
            pair.touch(now);
        }
    }
}

pub struct LocalWatcher {
    watcher: Option<RecommendedWatcher>,
    hints: Arc<Mutex<Hints>>,
    /// Watched roots and the (device, inode) they had when the watch began.
    watches: HashMap<PathBuf, (u64, u64)>,
    /// The folders below its root that each pair watches.
    folders: HashMap<i32, (PathBuf, HashSet<PathBuf>)>,
    /// How many roots and pair folders use each inotify watch: nested
    /// pairs and pairs sharing a root watch the same folders.
    users: HashMap<PathBuf, usize>,
    /// Pairs whose folders could not all be watched (the watch limit).
    incomplete: HashSet<i32>,
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
            folders: HashMap::new(),
            users: HashMap::new(),
            incomplete: HashSet::new(),
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
                    internal: None,
                    dirty: true,
                    burst: None,
                    verified_at: None,
                });
                if pair.root != *root {
                    pair.root = root.clone();
                    pair.verified_at = None;
                }
            }
        }
        let gone: Vec<i32> = self
            .folders
            .keys()
            .filter(|id| !pairs.iter().any(|(pair, _)| pair == *id))
            .copied()
            .collect();
        for id in gone {
            self.forget_folders(id);
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
            && !self.incomplete.contains(&id)
            && !pair.dirty
            && pair
                .verified_at
                .is_some_and(|at| at.elapsed() < FULL_SCAN_INTERVAL);
        if !unchanged {
            pair.dirty = false;
            pair.burst = None;
            pair.verified_at = None;
        }
        unchanged
    }

    /// Events inside `path` (the pair's internal folder) are Celestial's own
    /// staging and recovery writes, not changes to synchronize.
    pub fn ignore_internal(&mut self, id: i32, path: PathBuf) {
        if let Some(pair) = lock(&self.hints).pairs.get_mut(&id) {
            pair.internal = Some(path);
        }
    }

    /// Whether the pair is in the middle of a burst of events: wait for it to
    /// end (or reach [`MAX_DELAY`]) before scanning.
    pub fn settling(&self, id: i32, now: Instant) -> bool {
        lock(&self.hints)
            .pairs
            .get(&id)
            .and_then(|pair| pair.burst)
            .is_some_and(|(first, last)| {
                now.duration_since(last) < QUIET_WINDOW && now.duration_since(first) < MAX_DELAY
            })
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
        match add_watch(watcher, &mut self.users, root) {
            Ok(()) => {
                self.watches.insert(root.clone(), identity);
                self.retry_after.remove(root);
                // Events before this point were missed: require a full scan.
                for pair in lock(&self.hints).pairs.values_mut() {
                    if pair.root == *root {
                        pair.dirty = true;
                        pair.burst = None;
                        pair.verified_at = None;
                    }
                }
            }
            Err(err) => {
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
        if self.watches.remove(root).is_none() {
            return;
        }
        let ids: Vec<i32> = self
            .folders
            .iter()
            .filter(|(_, (folders_root, _))| folders_root == root)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.forget_folders(id);
        }
        if let Some(watcher) = self.watcher.as_mut() {
            remove_watch(watcher, &mut self.users, root);
        }
    }

    /// Watch exactly `folders` (the folders a full scan of the pair
    /// included) below its watched root. Events in a newly watched folder
    /// may have been missed between the scan and the watch, so adding one
    /// requires another scan; it finds the same folders and settles.
    pub fn watch_folders(&mut self, id: i32, root: &Path, folders: Vec<PathBuf>) {
        if !self.watches.contains_key(root) {
            return;
        }
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        let wanted: HashSet<PathBuf> = folders.into_iter().filter(|dir| dir != root).collect();
        let mut current = match self.folders.remove(&id) {
            Some((watched_root, current)) if watched_root == root => current,
            Some((_, old)) => {
                for dir in &old {
                    remove_watch(watcher, &mut self.users, dir);
                }
                HashSet::new()
            }
            None => HashSet::new(),
        };
        for dir in current.difference(&wanted).cloned().collect::<Vec<_>>() {
            remove_watch(watcher, &mut self.users, &dir);
            current.remove(&dir);
        }
        let mut added = false;
        let mut complete = true;
        for dir in wanted {
            if current.contains(&dir) {
                continue;
            }
            match add_watch(watcher, &mut self.users, &dir) {
                Ok(()) => {
                    current.insert(dir);
                    added = true;
                }
                Err(err) if matches!(err.kind, notify::ErrorKind::MaxFilesWatch) => {
                    tracing::warn!(root = %root.display(), watched = current.len(), "inotify watch limit reached; scanning instead");
                    complete = false;
                    break;
                }
                // Unreadable or already gone: the scan reports it.
                Err(err) => tracing::debug!(dir = %dir.display(), "folder not watched: {err}"),
            }
        }
        self.folders.insert(id, (root.to_path_buf(), current));
        if complete {
            self.incomplete.remove(&id);
        } else {
            self.incomplete.insert(id);
        }
        if added {
            if let Some(pair) = lock(&self.hints).pairs.get_mut(&id) {
                pair.dirty = true;
                pair.verified_at = None;
            }
        }
    }

    fn forget_folders(&mut self, id: i32) {
        self.incomplete.remove(&id);
        let Some((_, folders)) = self.folders.remove(&id) else {
            return;
        };
        if let Some(watcher) = self.watcher.as_mut() {
            for dir in &folders {
                remove_watch(watcher, &mut self.users, dir);
            }
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
        self.folders.clear();
        self.users.clear();
        self.incomplete.clear();
        self.retry_after.clear();
        self.broken_since = None;
        self.watcher = new_watcher(&self.hints);
        let mut hints = lock(&self.hints);
        hints.broken = self.watcher.is_none();
        hints.mark_all();
    }
}

fn add_watch(
    watcher: &mut RecommendedWatcher,
    users: &mut HashMap<PathBuf, usize>,
    dir: &Path,
) -> notify::Result<()> {
    if let Some(count) = users.get_mut(dir) {
        *count += 1;
        return Ok(());
    }
    watcher.watch(dir, RecursiveMode::NonRecursive)?;
    users.insert(dir.to_path_buf(), 1);
    Ok(())
}

fn remove_watch(watcher: &mut RecommendedWatcher, users: &mut HashMap<PathBuf, usize>, dir: &Path) {
    let Some(count) = users.get_mut(dir) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        users.remove(dir);
        // A removed folder's watch is already gone.
        let _ = watcher.unwatch(dir);
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
            let now = Instant::now();
            for path in &event.paths {
                if root_lost && hints.pairs.values().any(|pair| pair.root == *path) {
                    hints.lost_roots.insert(path.clone());
                }
                let mut matched = false;
                // Every pair whose root contains the path is affected: pairs
                // sharing a root, nested roots, and a change to a pair's
                // exclusion rules.
                for pair in hints.pairs.values_mut() {
                    if path.starts_with(&pair.root) {
                        matched = true;
                        if pair
                            .internal
                            .as_ref()
                            .is_some_and(|internal| path.starts_with(internal))
                        {
                            continue;
                        }
                        pair.touch(now);
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

    /// Every folder below `root`, as a full scan would include them.
    fn folders_of(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    found.push(path.clone());
                    pending.push(path);
                }
            }
        }
        found
    }

    /// A pair whose scan included `folders`, verified and quiet.
    fn watched_with(root: &Path, folders: Vec<PathBuf>) -> LocalWatcher {
        let mut watcher = LocalWatcher::new();
        watcher.sync_pairs(&[(1, root.to_path_buf())]);
        assert!(!watcher.local_unchanged(1, root), "first check must scan");
        watcher.watch_folders(1, root, folders);
        // Newly watched folders require one more scan.
        assert!(!watcher.local_unchanged(1, root));
        watcher.mark_verified(1);
        assert!(watcher.local_unchanged(1, root));
        watcher
    }

    fn watched(root: &Path) -> LocalWatcher {
        watched_with(root, folders_of(root))
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
        // The scan that follows includes the new folder.
        watcher.watch_folders(1, temp.path(), folders_of(temp.path()));
        watcher.local_unchanged(1, temp.path());
        watcher.mark_verified(1);
        fs::write(temp.path().join("new/file.txt"), "edit").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
    }

    #[test]
    fn excluded_folders_are_not_watched() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("kept/inner")).unwrap();
        fs::create_dir_all(temp.path().join("cache/deep")).unwrap();
        let included = vec![temp.path().join("kept"), temp.path().join("kept/inner")];
        let mut watcher = watched_with(temp.path(), included);
        assert_eq!(watcher.users.len(), 3, "the root and the two kept folders");
        fs::write(temp.path().join("cache/deep/file"), "x").unwrap();
        fs::write(temp.path().join("cache/file"), "x").unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(watcher.local_unchanged(1, temp.path()));
        fs::write(temp.path().join("kept/inner/file"), "x").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
        // A folder the next scan no longer includes is no longer watched.
        watcher.watch_folders(1, temp.path(), vec![temp.path().join("kept")]);
        assert_eq!(watcher.users.len(), 2);
    }

    #[test]
    fn unreadable_folders_do_not_stop_the_others() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let locked = temp.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::create_dir(temp.path().join("open")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // The scan lists the unreadable folder too; its watch fails alone.
        let mut watcher = watched_with(temp.path(), vec![locked.clone(), temp.path().join("open")]);
        fs::write(temp.path().join("open/file"), "x").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn nested_pairs_share_watches() {
        let temp = tempfile::tempdir().unwrap();
        let inner = temp.path().join("inner");
        fs::create_dir_all(inner.join("deep")).unwrap();
        let mut watcher = LocalWatcher::new();
        watcher.sync_pairs(&[(1, temp.path().to_path_buf()), (2, inner.clone())]);
        watcher.watch_folders(1, temp.path(), folders_of(temp.path()));
        watcher.watch_folders(2, &inner, folders_of(&inner));
        assert_eq!(watcher.users.len(), 3);
        assert_eq!(watcher.users[&inner.join("deep")], 2);
        // Removing the outer pair keeps the inner pair's watches.
        watcher.sync_pairs(&[(2, inner.clone())]);
        assert_eq!(watcher.users.len(), 2);
        assert_eq!(watcher.users[&inner.join("deep")], 1);
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
    fn an_event_during_a_scan_leaves_the_pair_dirty() {
        let temp = tempfile::tempdir().unwrap();
        let mut watcher = watched(temp.path());
        fs::write(temp.path().join("a"), "x").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
        // The scan starts (the check above), an edit lands while it runs,
        // and the scan then reports the tree verified.
        fs::write(temp.path().join("b"), "during the scan").unwrap();
        thread::sleep(Duration::from_millis(200));
        watcher.mark_verified(1);
        assert!(!watcher.local_unchanged(1, temp.path()));
    }

    #[test]
    fn bursts_are_debounced_with_a_maximum_delay() {
        let temp = tempfile::tempdir().unwrap();
        let mut watcher = watched(temp.path());
        let start = Instant::now();
        {
            let mut hints = lock(&watcher.hints);
            let pair = hints.pairs.get_mut(&1).unwrap();
            pair.touch(start);
        }
        assert!(watcher.settling(1, start + Duration::from_millis(100)));
        assert!(!watcher.settling(1, start + QUIET_WINDOW));
        {
            let mut hints = lock(&watcher.hints);
            let pair = hints.pairs.get_mut(&1).unwrap();
            pair.burst = Some((start, start + Duration::from_millis(1900)));
        }
        // Still writing, but the burst has lasted long enough.
        assert!(watcher.settling(1, start + Duration::from_millis(1950)));
        assert!(!watcher.settling(1, start + MAX_DELAY));
        assert!(!watcher.local_unchanged(1, temp.path()));
        assert!(!watcher.settling(1, start + Duration::from_millis(1950)));
    }

    #[test]
    fn internal_folder_writes_do_not_mark_the_pair() {
        let temp = tempfile::tempdir().unwrap();
        let internal = temp.path().join(".celestial-test");
        fs::create_dir(&internal).unwrap();
        let mut watcher = watched(temp.path());
        watcher.ignore_internal(1, internal.clone());
        fs::write(internal.join("staged"), "x").unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(watcher.local_unchanged(1, temp.path()));
        fs::write(temp.path().join(model_rules()), "*.tmp").unwrap();
        assert!(wait_for_change(&mut watcher, temp.path()));
    }

    fn model_rules() -> &'static str {
        crate::sync::model::FILE_IGNORE_NAME
    }

    #[test]
    fn nested_roots_are_both_marked() {
        let temp = tempfile::tempdir().unwrap();
        let inner = temp.path().join("inner");
        fs::create_dir(&inner).unwrap();
        let mut watcher = LocalWatcher::new();
        watcher.sync_pairs(&[(1, temp.path().to_path_buf()), (2, inner.clone())]);
        for (id, root) in [(1, temp.path()), (2, inner.as_path())] {
            assert!(!watcher.local_unchanged(id, root));
            watcher.mark_verified(id);
        }
        thread::sleep(Duration::from_millis(100));
        fs::write(inner.join("file"), "x").unwrap();
        thread::sleep(Duration::from_millis(300));
        assert!(!watcher.local_unchanged(1, temp.path()));
        assert!(!watcher.local_unchanged(2, &inner));
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
