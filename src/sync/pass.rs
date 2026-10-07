//! One synchronization pass over a pair.
//!
//! A pass observes the whole local tree, lists the remote folder once (with
//! provider hashes) and reads the pair's history once. Every item then goes
//! through the single decision in [`super::reconcile`]. One item failing or
//! conflicting never stops the others: it is retried with backoff or blocked
//! with a visible reason. Only problems with the whole pair (an unavailable
//! or emptied root, unreadable exclusions, a database failure) stop a pass.

use super::capabilities::Capabilities;
use super::failpoint;
use super::localfs::Root;
use super::model::{FILE_IGNORE_NAME, SyncError};
use super::observe::{self, Fingerprint, HashError, LocalMeta, LocalTree, Obs, RemoteMeta};
use super::reconcile::{self, ConflictKind, Decision, Delta, History, Input, Kind, Side};
use super::recovery::{self, Internal, Store as Recovery};
use super::store::{self, Baseline, BaselineWrite};
use crate::entities::{RemotesModel, SyncDirsModel};
use crate::rclone::{self, ListOptions, RcloneListFilter};
use sea_orm::DatabaseConnection;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const LOCAL_EMPTIED: &str = "Every synced item is missing from the local folder while the remote still has them. Automatic deletion is paused; check that the local folder or its drive is mounted. To delete everything intentionally, remove the items on both sides.";
pub const REMOTE_EMPTIED: &str = "Every synced item is missing from the remote folder while the local folder still has them. Automatic deletion is paused; check the remote. To delete everything intentionally, remove the items on both sides.";
const CHANGING: &str =
    "The file is still being changed; it will be synchronized once it is stable.";

/// The pair a pass works on.
pub struct Pair<'a> {
    pub db: &'a DatabaseConnection,
    pub remote: &'a RemotesModel,
    pub dir: &'a SyncDirsModel,
    pub caps: &'a Capabilities,
}

/// Callbacks into the worker between items.
pub trait Hooks {
    /// Process pending commands. `false` aborts the pass without further
    /// writes (quit, or the pair was stopped).
    fn checkpoint(&mut self) -> bool;
    fn progress(&mut self, text: String);
}

#[derive(Debug, Default)]
pub struct Report {
    /// Transfers, deletions and baselines written.
    pub actions: usize,
    /// Items waiting for an automatic retry.
    pub retrying: usize,
    /// Items that need the person to change something.
    pub blocked: usize,
    /// When the earliest retry is due.
    pub next_retry: Option<i64>,
    /// Item issues to show, keyed by relative path.
    pub issues: Vec<(String, SyncError)>,
    pub aborted: bool,
    /// The local observation, reusable while the watcher sees no change.
    pub local_tree: Option<LocalTree>,
    pub stats: Stats,
}

/// What a pass cost, for measurement and logging.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub local_items: usize,
    pub remote_items: usize,
    /// Bytes of local files read to compute hashes.
    pub hashed_bytes: u64,
    /// rclone RPCs: listings, stats, transfers and moves.
    pub rpc_calls: u64,
    /// Whether the local tree was walked (rather than reused).
    pub walked: bool,
    pub millis: u128,
}

impl Report {
    /// The pass changed nothing, so its local observation stays valid until
    /// the watcher reports an event. Items waiting for a retry are retried on
    /// their own schedule.
    pub fn settled(&self) -> bool {
        self.actions == 0 && !self.aborted
    }
}

struct Entry {
    rel: String,
    local: Obs<LocalMeta>,
    remote: Obs<RemoteMeta>,
    base: Option<Baseline>,
    decision: Decision,
    /// A local fingerprint computed while deciding, reused when recording.
    fingerprint: Option<Fingerprint>,
    /// Waiting for a scheduled retry: decided again only when due.
    deferred: bool,
    /// Items below this one are not touched in this pass.
    skip_children: bool,
    /// Why the item is blocked, when it is.
    blocked: Option<String>,
    /// An excess conflict copy to move into recovery on both sides.
    prune: bool,
}

impl Pair<'_> {
    fn local_path(&self, rel: &str) -> PathBuf {
        Path::new(&self.dir.local_path).join(rel)
    }

    fn remote_path(&self, rel: &str) -> String {
        store::item_paths(self.dir, rel).1
    }

    /// The fingerprint algorithm: the provider's hash, or MD5 computed
    /// locally when the provider has none.
    fn algo(&self) -> &str {
        self.caps.preferred_hash().unwrap_or("md5")
    }

    fn list_options(&self) -> ListOptions {
        ListOptions {
            hash_types: self
                .caps
                .preferred_hash()
                .map(|hash| vec![hash.to_owned()])
                .unwrap_or_default(),
            use_list_r: self.caps.list_r,
        }
    }

    fn stat_remote(&self, rel: &str) -> Result<Option<RemoteMeta>, String> {
        rclone::sync::stat_with(
            &self.remote.name,
            &self.remote_path(rel),
            &self.list_options(),
        )
        .map(|item| item.map(|item| RemoteMeta::from_item(&item, self.caps.preferred_hash())))
        .map_err(|err| err.error)
    }

    /// A remote hash is a fingerprint only in the pair's algorithm.
    fn remote_fingerprint(&self, meta: &RemoteMeta) -> Option<Fingerprint> {
        let algo = self.caps.preferred_hash()?;
        meta.hash.as_ref().map(|value| Fingerprint {
            algo: algo.to_owned(),
            value: value.clone(),
        })
    }

    fn hash_local(&self, rel: &str, algo: &str) -> Result<Fingerprint, String> {
        match observe::hash_local(&self.local_path(rel), algo) {
            Ok(Some(fp)) => Ok(fp),
            Ok(None) => Err(format!(
                "Could not compute the {algo} hash of the local file"
            )),
            Err(HashError::Changing) => Err(CHANGING.into()),
            Err(HashError::Failed(err)) => Err(err),
        }
    }

    fn local_delta(
        &self,
        rel: &str,
        meta: &LocalMeta,
        base: &Baseline,
    ) -> (Delta, Option<Fingerprint>) {
        if base.is_dir != Some(meta.is_dir) {
            return (Delta::Changed, None);
        }
        if meta.is_dir {
            return (Delta::Same, None);
        }
        if base.local.is_some_and(|known| known.same_tuple(meta)) {
            return (Delta::Same, None);
        }
        if base
            .size
            .is_some_and(|size| u64::try_from(size).ok() != Some(meta.size))
        {
            return (Delta::Changed, None);
        }
        let Some(expected) = &base.fingerprint else {
            return (Delta::Changed, None);
        };
        match self.hash_local(rel, &expected.algo) {
            Ok(fp) if fp.value == expected.value => (Delta::MetadataOnly, Some(fp)),
            Ok(fp) => (Delta::Changed, Some(fp)),
            Err(reason) => (Delta::Unknown(reason), None),
        }
    }

    fn sides_delta(
        &self,
        rel: &str,
        local: &LocalMeta,
        remote: &RemoteMeta,
    ) -> (Delta, Option<Fingerprint>) {
        if local.is_dir != remote.is_dir {
            return (Delta::Changed, None);
        }
        if local.is_dir {
            return (Delta::Same, None);
        }
        // Different sizes prove different contents without hashing.
        if remote.size >= 0 && u64::try_from(remote.size).ok() != Some(local.size) {
            return (Delta::Changed, None);
        }
        if let Some(remote_fp) = self.remote_fingerprint(remote) {
            return match self.hash_local(rel, &remote_fp.algo) {
                Ok(fp) if fp.value.eq_ignore_ascii_case(&remote_fp.value) => {
                    (Delta::Same, Some(fp))
                }
                Ok(fp) => (Delta::Changed, Some(fp)),
                Err(reason) => (Delta::Unknown(reason), None),
            };
        }
        // Without a provider hash, equal size and modification time (within
        // the provider's precision) is the only evidence available.
        if local
            .mtime
            .same_within(remote.mtime, self.caps.precision_ns)
        {
            (Delta::Same, None)
        } else {
            (Delta::Changed, None)
        }
    }
}

fn remote_delta(meta: &RemoteMeta, base: &Baseline) -> Delta {
    if base.is_dir != Some(meta.is_dir) {
        return Delta::Changed;
    }
    if meta.is_dir {
        return Delta::Same;
    }
    if meta.size >= 0
        && base
            .remote_size
            .is_some_and(|size| size >= 0 && size != meta.size)
    {
        return Delta::Changed;
    }
    match (&meta.hash, &base.remote_hash) {
        (Some(hash), Some(known)) if hash == known => {
            if meta.mtime == base.remote_mtime {
                Delta::Same
            } else {
                Delta::MetadataOnly
            }
        }
        (Some(_), Some(_)) => Delta::Changed,
        _ if meta.mtime == base.remote_mtime && Some(meta.size) == base.remote_size => Delta::Same,
        _ => Delta::Changed,
    }
}

/// What a pass needs besides the pair: its internal folder and lazily read
/// remote free space.
struct Cx<'a> {
    pair: &'a Pair<'a>,
    internal: Internal,
    /// The pair root, opened once and checked against its identity.
    root: Root,
    remote_free: Cell<Option<Option<i64>>>,
}

impl Cx<'_> {
    fn rec(&self) -> Recovery<'_> {
        Recovery {
            db: self.pair.db,
            remote: self.pair.remote,
            dir: self.pair.dir,
            internal: &self.internal,
            root: &self.root,
        }
    }

    fn stat_local(&self, rel: &str) -> Result<Option<LocalMeta>, String> {
        self.root.lstat(rel).map_err(|err| err.to_string())
    }

    /// Pause before writing when the destination is short of space, instead
    /// of filling it or dropping preservation.
    fn check_space(&self, local: bool, size: i64) -> Result<(), String> {
        let needed = size.max(0).saturating_add(1 << 20);
        if local {
            let stat =
                nix::sys::statvfs::statvfs(&self.internal.path).map_err(|err| err.to_string())?;
            let free = i64::try_from(stat.blocks_available().saturating_mul(stat.fragment_size()))
                .unwrap_or(i64::MAX);
            if free < needed {
                return Err(tr::tr!(
                    "Not enough free space in the local folder; waiting for space."
                ));
            }
            return Ok(());
        }
        if self.remote_free.get().is_none() {
            let free = rclone::sync::free_space(&self.pair.remote.name).unwrap_or(None);
            self.remote_free.set(Some(free));
        }
        match self.remote_free.get().flatten() {
            Some(free) if free < needed => Err(tr::tr!(
                "Not enough free space on the remote; waiting for space."
            )),
            _ => Ok(()),
        }
    }
}

/// Run one pass. `cached` is a local observation the watcher vouches for.
pub fn run(
    pair: &Pair,
    hooks: &mut dyn Hooks,
    cached: Option<LocalTree>,
) -> Result<Report, String> {
    let started = std::time::Instant::now();
    let rpc_before = rclone::sync::RPC_CALLS.load(std::sync::atomic::Ordering::Relaxed);
    let hashed_before = observe::HASHED_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let walked = cached.is_none();
    let root_path = Path::new(&pair.dir.local_path);
    let patterns = load_ignore_globs(&root_path.join(FILE_IGNORE_NAME))?;
    let cx = context(pair)?;
    // Finish or undo anything an interruption left half done before looking.
    cx.rec().replay()?;
    let internal = &cx.internal;
    // When the whole remote is synced, its recovery folder is inside it.
    let root_pair = pair.dir.remote_path.is_empty();
    let ours_remote = |rel: &str| {
        root_pair
            && (rel == recovery::REMOTE_RECOVERY_ROOT
                || rel.starts_with(&format!("{}/", recovery::REMOTE_RECOVERY_ROOT)))
    };
    let ignored = |rel: &str| is_ignored(rel, &patterns);
    let local = match cached {
        Some(tree) => tree,
        None => observe::observe_local(root_path, &|rel, metadata| {
            ignored(rel) || internal.is(metadata)
        })
        .map_err(|err| format!("Local sync folder is unavailable: {err}"))?,
    };
    // The walk went by path: make sure it walked the folder that is open.
    let identity = cx.root.identity().map_err(|err| err.to_string())?;
    Root::open(root_path, Some(identity)).map_err(|err| err.to_string())?;
    let listing = rclone::sync::list_with(
        &pair.remote.name,
        &pair.dir.remote_path,
        true,
        RcloneListFilter::All,
        &pair.list_options(),
    )
    .map_err(|err| err.error)?;
    let remote = observe::remote_tree(
        &pair.dir.remote_path,
        &listing,
        pair.caps.preferred_hash(),
        &|rel| ignored(rel) || ours_remote(rel),
    );
    let mut baselines = store::load_baselines(pair.db, pair.dir).map_err(|err| err.to_string())?;
    baselines.retain(|rel, _| !ignored(rel));
    let states = store::load_item_states(pair.db, pair.dir.id).map_err(|err| err.to_string())?;

    // An unmounted drive or a provider returning an empty listing looks like
    // a folder whose every item was deleted. Never propagate that wholesale.
    let tracked_local = baselines.keys().any(|rel| local.items.contains_key(rel));
    let tracked_remote = baselines.keys().any(|rel| remote.items.contains_key(rel));
    if local.items.is_empty() && tracked_remote {
        return Err(LOCAL_EMPTIED.into());
    }
    if remote.items.is_empty() && tracked_local {
        return Err(REMOTE_EMPTIED.into());
    }

    let mut blocked: BTreeMap<String, String> = local
        .blocked
        .iter()
        .chain(remote.blocked.iter())
        .map(|(rel, reason)| (rel.clone(), reason.clone()))
        .collect();
    blocked.extend(observe::collisions(
        local
            .items
            .keys()
            .chain(remote.items.keys())
            .map(String::as_str),
        pair.caps.case_insensitive,
    ));
    let reserved = tr::tr!("This name is reserved for Celestial's preserved versions. Rename it.");
    for rel in remote.items.keys() {
        if rel == &internal.name || rel.starts_with(&format!("{}/", internal.name)) {
            blocked.insert(rel.clone(), reserved.clone());
        }
    }
    if root_pair && local.items.contains_key(recovery::REMOTE_RECOVERY_ROOT) {
        blocked.insert(recovery::REMOTE_RECOVERY_ROOT.into(), reserved.clone());
    }

    let rels: BTreeSet<&String> = local
        .items
        .keys()
        .chain(remote.items.keys())
        .chain(baselines.keys())
        .chain(blocked.keys())
        .collect();
    // Order by path components so every folder is directly followed by its
    // contents ("a", "a/b", "a b"), not interleaved with siblings.
    let mut rels: Vec<String> = rels.into_iter().cloned().collect();
    rels.sort_by(|left, right| left.split('/').cmp(right.split('/')));

    let now = store::now();
    let mut entries: Vec<Entry> = Vec::with_capacity(rels.len());
    let mut skip_under: Option<String> = None;
    for (index, rel) in rels.into_iter().enumerate() {
        // Deciding can hash files: stay responsive to stop and quit.
        if index % 64 == 63 && !hooks.checkpoint() {
            return Ok(Report {
                aborted: true,
                ..Report::default()
            });
        }
        if let Some(parent) = &skip_under {
            if rel.starts_with(&format!("{parent}/")) {
                continue;
            }
            skip_under = None;
        }
        let mut entry = Entry {
            local: local.observe(&rel),
            remote: remote.observe(&rel),
            base: baselines.get(&rel).cloned(),
            decision: Decision::InSync,
            fingerprint: None,
            deferred: false,
            skip_children: false,
            blocked: blocked.get(&rel).cloned(),
            prune: false,
            rel,
        };
        if entry.blocked.is_some() {
            entry.skip_children = true;
            skip_under = Some(entry.rel.clone());
            entries.push(entry);
            continue;
        }
        if let Some(state) = states.get(&entry.rel) {
            if state.state == store::RETRYING && state.next_attempt.is_some_and(|at| at > now) {
                entry.deferred = true;
                entry.decision = Decision::Retry(state.reason.clone());
                entries.push(entry);
                continue;
            }
        }
        decide(pair, &mut entry);
        if matches!(entry.decision, Decision::KeepBoth(ConflictKind::FileFolder))
            || matches!(entry.local, Obs::Unknown(_))
            || matches!(entry.remote, Obs::Unknown(_))
        {
            entry.skip_children = true;
            skip_under = Some(entry.rel.clone());
        }
        entries.push(entry);
    }
    settle_folders(&mut entries);
    cap_conflict_copies(&mut entries);

    let mut report = Report::default();
    let mut failed = vec![false; entries.len()];
    // Removals first, deepest items first, so folders are empty when they go.
    for index in (0..entries.len()).rev() {
        if !hooks.checkpoint() {
            report.aborted = true;
            return Ok(report);
        }
        let entry = &entries[index];
        if entry.deferred || entry.blocked.is_some() {
            continue;
        }
        let result = match removal(entry) {
            Some(step) => {
                hooks.progress(tr::tr!("Synchronizing '{}'...", entry.rel));
                execute_removal(&cx, entry, step).map(|()| report.actions += 1)
            }
            None => continue,
        };
        if let Err(reason) = result {
            failed[index] = true;
            retry(pair, &entry.rel, &reason)?;
        }
    }
    // Then creations and transfers, parents before their contents.
    for (index, entry) in entries.iter().enumerate() {
        if !hooks.checkpoint() {
            report.aborted = true;
            return Ok(report);
        }
        if failed[index] || entry.deferred || entry.prune {
            continue;
        }
        if let Some(reason) = &entry.blocked {
            store::set_blocked(pair.db, pair.dir.id, &entry.rel, reason)
                .map_err(|err| err.to_string())?;
            continue;
        }
        let outcome = match &entry.decision {
            Decision::Retry(reason) => Err(reason.clone()),
            Decision::InSync | Decision::Nothing => Ok(false),
            Decision::DeleteLocal | Decision::DeleteRemote => Ok(false),
            decision => {
                hooks.progress(tr::tr!("Synchronizing '{}'...", entry.rel));
                execute(&cx, entry, decision).map(|()| true)
            }
        };
        match outcome {
            Ok(acted) => {
                report.actions += usize::from(acted);
                store::clear_item_state(pair.db, pair.dir.id, &entry.rel)
                    .map_err(|err| err.to_string())?;
            }
            Err(reason) => retry(pair, &entry.rel, &reason)?,
        }
    }
    // Forget states of items that no longer exist anywhere.
    let present: BTreeSet<&str> = entries.iter().map(|entry| entry.rel.as_str()).collect();
    for rel in states.keys() {
        if !present.contains(rel.as_str()) && !blocked_ancestor(&entries, rel) {
            store::clear_item_state(pair.db, pair.dir.id, rel).map_err(|err| err.to_string())?;
        }
    }
    let live: HashSet<String> = local
        .items
        .keys()
        .chain(remote.items.keys())
        .cloned()
        .collect();
    if let Err(err) = cx.rec().prune(&live) {
        tracing::warn!("could not apply the retention policy: {err}");
    }
    // Finished journal entries are kept a week for diagnosis.
    if let Err(err) = store::prune_operations(pair.db, 7 * 24 * 3600) {
        tracing::warn!("could not prune the operation journal: {err}");
    }

    let states = store::load_item_states(pair.db, pair.dir.id).map_err(|err| err.to_string())?;
    for state in states.values() {
        if state.state == store::RETRYING {
            report.retrying += 1;
            report.next_retry = Some(
                report
                    .next_retry
                    .map_or(state.next_attempt.unwrap_or(now), |at| {
                        at.min(state.next_attempt.unwrap_or(now))
                    }),
            );
        } else {
            report.blocked += 1;
        }
        report.issues.push((
            state.rel.clone(),
            SyncError::Item {
                rel: state.rel.clone(),
                path: pair.local_path(&state.rel).display().to_string(),
                state: state.state.clone(),
                reason: state.reason.clone(),
                next_attempt: state.next_attempt,
            },
        ));
    }
    for record in store::open_conflicts(pair.db, pair.dir.id).map_err(|err| err.to_string())? {
        report.issues.push((
            format!("conflict:{}", record.id),
            SyncError::Conflict {
                id: record.id,
                rel: record.rel,
                kind: record.kind,
                detail: record.detail,
            },
        ));
    }
    report.stats = Stats {
        local_items: local.items.len(),
        remote_items: remote.items.len(),
        hashed_bytes: observe::HASHED_BYTES.load(std::sync::atomic::Ordering::Relaxed)
            - hashed_before,
        rpc_calls: rclone::sync::RPC_CALLS.load(std::sync::atomic::Ordering::Relaxed) - rpc_before,
        walked,
        millis: started.elapsed().as_millis(),
    };
    tracing::debug!(pair = %pair.dir.local_path, stats = ?report.stats, actions = report.actions, "pass finished");
    report.local_tree = Some(local);
    Ok(report)
}

/// Keep at most [`recovery::MAX_CONFLICT_COPIES`] visible conflict copies per
/// item. Older copies go to recovery on both sides, but only copies that are
/// verified identical on both sides.
fn cap_conflict_copies(entries: &mut [Entry]) {
    let mut groups: BTreeMap<String, Vec<(String, usize)>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some((origin, stamp)) = recovery::conflict_origin(&entry.rel) {
            groups.entry(origin).or_default().push((stamp, index));
        }
    }
    for mut copies in groups.into_values() {
        if copies.len() <= recovery::MAX_CONFLICT_COPIES {
            continue;
        }
        copies.sort_by(|left, right| right.cmp(left));
        for (_, index) in copies.into_iter().skip(recovery::MAX_CONFLICT_COPIES) {
            let entry = &mut entries[index];
            let verified = entry.base.as_ref().is_some_and(Baseline::verified);
            let files = entry.local.present().is_some_and(|meta| !meta.is_dir)
                && entry.remote.present().is_some_and(|meta| !meta.is_dir);
            if verified && files && entry.decision == Decision::InSync && !entry.deferred {
                entry.prune = true;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Removal {
    LocalFile,
    LocalDir,
    RemoteFile,
    RemoteDir,
    /// An excess conflict copy, removed from both sides into recovery.
    Both,
}

/// The removal an entry needs before anything is created.
fn removal(entry: &Entry) -> Option<Removal> {
    if entry.prune {
        return Some(Removal::Both);
    }
    let local_dir = entry.local.present().map(|meta| meta.is_dir);
    let remote_dir = entry.remote.present().map(|meta| meta.is_dir);
    match entry.decision {
        Decision::DeleteLocal => Some(if local_dir? {
            Removal::LocalDir
        } else {
            Removal::LocalFile
        }),
        Decision::DeleteRemote => Some(if remote_dir? {
            Removal::RemoteDir
        } else {
            Removal::RemoteFile
        }),
        // A folder replaced by a file is emptied by its children's removals.
        Decision::Download if local_dir == Some(true) && remote_dir == Some(false) => {
            Some(Removal::LocalDir)
        }
        Decision::Upload if remote_dir == Some(true) && local_dir == Some(false) => {
            Some(Removal::RemoteDir)
        }
        _ => None,
    }
}

fn execute_removal(cx: &Cx, entry: &Entry, step: Removal) -> Result<(), String> {
    let pair = cx.pair;
    let deletion = matches!(
        entry.decision,
        Decision::DeleteLocal | Decision::DeleteRemote
    ) || entry.prune;
    let op = recovery::random_id();
    let reason = if entry.prune {
        tr::tr!("Older conflict copy")
    } else {
        tr::tr!("Deleted on the other side")
    };
    let local_step = matches!(step, Removal::LocalFile | Removal::LocalDir | Removal::Both);
    let remote_step = matches!(
        step,
        Removal::RemoteFile | Removal::RemoteDir | Removal::Both
    );
    // Observe both sides again immediately before removing anything.
    if local_step {
        let Obs::Present(expected) = &entry.local else {
            return Err("The local item is no longer present".into());
        };
        if !cx
            .stat_local(&entry.rel)?
            .is_some_and(|now| now.same_tuple(expected))
        {
            return Err(tr::tr!("The local item changed after it was checked."));
        }
    }
    if remote_step {
        let Obs::Present(expected) = &entry.remote else {
            return Err("The remote item is no longer present".into());
        };
        if !pair
            .stat_remote(&entry.rel)?
            .is_some_and(|now| same_remote(expected, &now))
        {
            return Err(tr::tr!("The remote item changed after it was checked."));
        }
    }
    if deletion && !entry.prune {
        let reappeared = if local_step {
            pair.stat_remote(&entry.rel)?.is_some()
        } else {
            cx.stat_local(&entry.rel)?.is_some()
        };
        if reappeared {
            return Err(tr::tr!(
                "The item reappeared on the other side after it was checked."
            ));
        }
    }
    if local_step {
        let Obs::Present(expected) = &entry.local else {
            return Err("The local item is no longer present".into());
        };
        if expected.is_dir {
            cx.root.rmdir(&entry.rel).map_err(|err| {
                tr::tr!(
                    "The folder still has contents that are not being deleted ({}).",
                    err
                )
            })?;
        } else {
            cx.rec()
                .preserve_local(&entry.rel, expected, &reason, &op)?;
        }
    }
    if remote_step {
        let Obs::Present(expected) = &entry.remote else {
            return Err("The remote item is no longer present".into());
        };
        if expected.is_dir {
            rclone::sync::rmdir(&pair.remote.name, &pair.remote_path(&entry.rel)).map_err(
                |err| {
                    tr::tr!(
                        "The remote folder still has contents that are not being deleted ({}).",
                        err.error
                    )
                },
            )?;
        } else {
            let (item, moved) = cx.rec().preserve_remote(
                &entry.rel,
                expected,
                &reason,
                &op,
                &pair.list_options(),
                pair.caps.preferred_hash(),
            )?;
            if !same_remote(expected, &moved) {
                // An edit arrived between the check and the move: it is
                // preserved, and stays visible as a conflict copy.
                keep_displaced_remote(cx, &entry.rel, &item, &moved, &op)?;
            }
        }
    }
    if deletion {
        if let Some(base) = &entry.base {
            store::delete_baseline(pair.db, base.id).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

fn execute(cx: &Cx, entry: &Entry, decision: &Decision) -> Result<(), String> {
    let pair = cx.pair;
    match decision {
        Decision::Upload | Decision::RestoreToRemote => {
            let Obs::Present(local) = &entry.local else {
                return Err("The local item is no longer present".into());
            };
            if local.is_dir {
                create_remote_dir(cx, entry, local)
            } else {
                upload(cx, entry, local)?;
                if *decision == Decision::RestoreToRemote {
                    note_restore(cx, entry, "remote")?;
                }
                Ok(())
            }
        }
        Decision::Download | Decision::RestoreToLocal => {
            let Obs::Present(remote) = &entry.remote else {
                return Err("The remote item is no longer present".into());
            };
            if remote.is_dir {
                create_local_dir(cx, entry, remote)
            } else {
                download(cx, entry, remote)?;
                if *decision == Decision::RestoreToLocal {
                    note_restore(cx, entry, "local")?;
                }
                Ok(())
            }
        }
        Decision::Record => record(pair, entry),
        Decision::KeepBoth(kind) => keep_both(cx, entry, *kind),
        Decision::Retire => match &entry.base {
            Some(base) => store::delete_baseline(pair.db, base.id).map_err(|err| err.to_string()),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Folder times change whenever their contents do; a folder is compared by
/// kind only, a file by everything the listing reports.
fn same_remote(seen: &RemoteMeta, now: &RemoteMeta) -> bool {
    if seen.is_dir || now.is_dir {
        seen.is_dir == now.is_dir
    } else {
        seen == now
    }
}

/// The local side must still be what was observed.
fn expect_local(cx: &Cx, entry: &Entry) -> Result<Option<LocalMeta>, String> {
    let now = cx.stat_local(&entry.rel)?;
    let same = match (&entry.local, &now) {
        (Obs::Present(seen), Some(now)) => seen.same_tuple(now),
        (Obs::Missing, None) => true,
        _ => false,
    };
    if same {
        Ok(now)
    } else {
        Err(tr::tr!("The local item changed after it was checked."))
    }
}

/// The remote side must still be what was observed.
fn expect_remote(pair: &Pair, entry: &Entry) -> Result<Option<RemoteMeta>, String> {
    let now = pair.stat_remote(&entry.rel)?;
    let same = match (&entry.remote, &now) {
        (Obs::Present(seen), Some(now)) => same_remote(seen, now),
        (Obs::Missing, None) => true,
        _ => false,
    };
    if same {
        Ok(now)
    } else {
        Err(tr::tr!("The remote item changed after it was checked."))
    }
}

/// An entry describing one path as it is now, for steps that create items
/// after an earlier step of the same operation.
fn fresh_entry(rel: &str, local: Option<LocalMeta>, remote: Option<RemoteMeta>) -> Entry {
    Entry {
        rel: rel.to_owned(),
        local: local.map_or(Obs::Missing, Obs::Present),
        remote: remote.map_or(Obs::Missing, Obs::Present),
        base: None,
        decision: Decision::InSync,
        fingerprint: None,
        deferred: false,
        skip_children: false,
        blocked: None,
        prune: false,
    }
}

/// Upload a local file. A remote file it replaces is moved into recovery
/// first (server-side where possible) and moved back if the upload fails.
/// rclone refuses a source whose size or time changes while it is read and
/// verifies a shared hash afterwards; the local tuple is compared before and
/// after for the same-size, same-time case rclone cannot see. Without a
/// shared hash the bytes are uploaded from a snapshot instead.
fn upload(cx: &Cx, entry: &Entry, local: &LocalMeta) -> Result<(), String> {
    let pair = cx.pair;
    expect_local(cx, entry)?;
    let replaced = if entry.remote.present().is_some_and(|remote| remote.is_dir) {
        // The folder this file replaces was emptied and removed first.
        if pair.stat_remote(&entry.rel)?.is_some() {
            return Err(tr::tr!("The remote folder could not be replaced yet."));
        }
        None
    } else {
        expect_remote(pair, entry)?
    };
    cx.check_space(false, i64::try_from(local.size).unwrap_or(i64::MAX))?;
    let destination = pair.remote_path(&entry.rel);
    let op = recovery::random_id();
    let rec = cx.rec();
    let snapshot = pair
        .caps
        .preferred_hash()
        .is_none()
        .then(|| rec.staging_rel(&op));
    rec.begin(
        &op,
        &entry.rel,
        "upload",
        serde_json::json!({ "remote": destination, "snapshot": snapshot }),
    )?;
    let result = upload_steps(
        cx,
        entry,
        local,
        replaced,
        &destination,
        &op,
        snapshot.as_deref(),
    );
    if let Some(snapshot) = &snapshot {
        let _ = cx.root.unlink(snapshot);
    }
    match result {
        Ok(()) => rec.finish(&op),
        Err(err) => {
            rec.abandon(&op, &err);
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn upload_steps(
    cx: &Cx,
    entry: &Entry,
    local: &LocalMeta,
    replaced: Option<RemoteMeta>,
    destination: &str,
    op: &str,
    snapshot: Option<&str>,
) -> Result<(), String> {
    let pair = cx.pair;
    let source_rel = match snapshot {
        Some(snapshot) => {
            fs::copy(pair.local_path(&entry.rel), cx.root.path().join(snapshot))
                .map_err(|err| err.to_string())?;
            if !cx
                .stat_local(&entry.rel)?
                .is_some_and(|now| now.same_tuple(local))
            {
                return Err(tr::tr!("The local file changed while it was read."));
            }
            snapshot.to_owned()
        }
        None => entry.rel.clone(),
    };
    let source = cx.root.path().join(&source_rel);
    let source = source.to_str().ok_or(observe::NON_UTF8)?;
    let preserved = match &replaced {
        Some(expected) => {
            let (item, moved) = cx.rec().preserve_remote(
                &entry.rel,
                expected,
                &tr::tr!("Replaced by the local version"),
                op,
                &pair.list_options(),
                pair.caps.preferred_hash(),
            )?;
            if !same_remote(expected, &moved) {
                keep_displaced_remote(cx, &entry.rel, &item, &moved, op)?;
            }
            cx.rec().step(
                op,
                "preserved",
                Some(&serde_json::json!({
                    "remote": destination,
                    "backup": item.stored_path,
                    "snapshot": snapshot,
                })),
            )?;
            Some(item)
        }
        None => None,
    };
    failpoint::hit("upload:before_transfer");
    if let Err(err) = rclone::sync::copy_to_remote(source, &pair.remote.name, destination) {
        if let Some(item) = preserved {
            // Put the previous version back where nothing replaced it.
            if matches!(pair.stat_remote(&entry.rel), Ok(None)) {
                let _ =
                    rclone::sync::move_on_remote(&pair.remote.name, &item.stored_path, destination);
            }
        }
        return Err(err.error);
    }
    failpoint::hit("upload:after_transfer");
    // Acknowledge only the bytes that were read: if the file changed while
    // it was uploaded, the next pass uploads it again.
    let after = cx
        .stat_local(&entry.rel)?
        .filter(|after| after.same_tuple(local))
        .ok_or_else(|| tr::tr!("The local file changed while it was uploaded."))?;
    let remote = pair
        .stat_remote(&entry.rel)?
        .ok_or_else(|| tr::tr!("The remote item disappeared after it was uploaded."))?;
    save(pair, &entry.rel, &after, &remote, None)
}

/// Download into the internal staging folder, then commit with an atomic
/// rename: no-replace for a new file, exchange for a replacement. The
/// displaced file is compared with what was observed and preserved; an edit
/// that arrived in between is kept as a conflict copy instead. The journal
/// records the staged inode so a replay can tell whether the commit
/// happened.
fn download(cx: &Cx, entry: &Entry, remote: &RemoteMeta) -> Result<(), String> {
    let pair = cx.pair;
    let replaced = if entry.local.present().is_some_and(|local| local.is_dir) {
        if cx.stat_local(&entry.rel)?.is_some() {
            return Err(tr::tr!("The local folder could not be replaced yet."));
        }
        None
    } else {
        expect_local(cx, entry)?
    };
    expect_remote(pair, entry)?;
    ensure_local_parent(cx, &entry.rel)?;
    cx.check_space(true, remote.size)?;
    let op = recovery::random_id();
    let rec = cx.rec();
    let stage = rec.staging_rel(&op);
    rec.begin(
        &op,
        &entry.rel,
        "download",
        serde_json::json!({ "stage": stage }),
    )?;
    let committed = Cell::new(false);
    match download_steps(cx, entry, remote, replaced, &op, &stage, &committed) {
        Ok(()) => rec.finish(&op),
        // After the exchange the stage may hold the displaced version: the
        // journal stays open so the next pass preserves it.
        Err(err) if committed.get() => Err(err),
        Err(err) => {
            rec.abandon(&op, &err);
            Err(err)
        }
    }
}

fn download_steps(
    cx: &Cx,
    entry: &Entry,
    remote: &RemoteMeta,
    replaced: Option<LocalMeta>,
    op: &str,
    stage: &str,
    committed: &Cell<bool>,
) -> Result<(), String> {
    let pair = cx.pair;
    let stage_path = cx.root.path().join(stage);
    let stage_text = stage_path.to_str().ok_or(observe::NON_UTF8)?;
    rclone::sync::copy_to_local(stage_text, &pair.remote.name, &pair.remote_path(&entry.rel))
        .map_err(|err| {
            let _ = cx.root.unlink(stage);
            err.error
        })?;
    let staged = cx
        .stat_local(stage)?
        .ok_or_else(|| tr::tr!("The downloaded file disappeared."))?;
    cx.root.sync_file(stage).map_err(|err| err.to_string())?;
    cx.rec().step(
        op,
        "staged",
        Some(&serde_json::json!({ "stage": stage, "stageInode": staged.inode })),
    )?;
    failpoint::hit("download:staged");
    if pair.stat_remote(&entry.rel)?.as_ref() != Some(remote) {
        let _ = cx.root.unlink(stage);
        return Err(tr::tr!("The remote file changed while it was downloaded."));
    }
    match replaced {
        None => match cx.root.rename_noreplace(stage, &entry.rel) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                // Something was created at the path meanwhile: keep both.
                let copy = recovery::conflict_rel(&entry.rel, "remote", op, observe::Stamp::now());
                cx.root
                    .rename_noreplace(stage, &copy)
                    .map_err(|err| err.to_string())?;
                record_conflict(
                    cx,
                    &entry.rel,
                    "concurrent_create",
                    &copy,
                    "remote",
                    op,
                    None,
                    Some(remote),
                )?;
                return Err(tr::tr!(
                    "A local item was created while the remote version was downloaded; both were kept."
                ));
            }
            Err(err) => {
                let _ = cx.root.unlink(stage);
                return Err(err.to_string());
            }
        },
        Some(expected) => {
            failpoint::hit("download:before_exchange");
            cx.root.exchange(stage, &entry.rel).map_err(|err| {
                let _ = cx.root.unlink(stage);
                tr::tr!("Could not replace the local file atomically [{}].", err)
            })?;
            committed.set(true);
            failpoint::hit("download:exchanged");
            // `stage` now holds the displaced version.
            let displaced = cx
                .stat_local(stage)?
                .ok_or_else(|| tr::tr!("The previous version disappeared."))?;
            if displaced.same_after_rename(&expected) {
                cx.rec().preserve_path(
                    stage,
                    &entry.rel,
                    &expected,
                    &tr::tr!("Replaced by the remote version"),
                    op,
                )?;
            } else {
                let copy = recovery::conflict_rel(&entry.rel, "local", op, observe::Stamp::now());
                cx.root
                    .rename_noreplace(stage, &copy)
                    .map_err(|err| err.to_string())?;
                record_conflict(
                    cx,
                    &entry.rel,
                    "edit_edit",
                    &copy,
                    "local",
                    op,
                    Some(&displaced),
                    Some(remote),
                )?;
            }
        }
    }
    cx.root
        .sync_file(&entry.rel)
        .map_err(|err| err.to_string())?;
    let local = cx
        .stat_local(&entry.rel)?
        .ok_or_else(|| tr::tr!("The local item disappeared after it was downloaded."))?;
    save(pair, &entry.rel, &local, remote, None)
}

fn create_remote_dir(cx: &Cx, entry: &Entry, local: &LocalMeta) -> Result<(), String> {
    let pair = cx.pair;
    expect_local(cx, entry)?;
    let path = pair.remote_path(&entry.rel);
    if let Some(remote) = expect_remote(pair, entry)? {
        if !remote.is_dir {
            let op = recovery::random_id();
            cx.rec().preserve_remote(
                &entry.rel,
                &remote,
                &tr::tr!("Replaced by a local folder"),
                &op,
                &pair.list_options(),
                pair.caps.preferred_hash(),
            )?;
        }
    }
    rclone::sync::mkdir(&pair.remote.name, &path).map_err(|err| err.error)?;
    match pair.stat_remote(&entry.rel)? {
        Some(remote) if remote.is_dir => save(pair, &entry.rel, local, &remote, None),
        // Backends without empty folders show the folder once it has contents.
        _ if !pair.caps.empty_dirs => Ok(()),
        _ => Err(tr::tr!(
            "The remote folder disappeared after it was created."
        )),
    }
}

fn create_local_dir(cx: &Cx, entry: &Entry, remote: &RemoteMeta) -> Result<(), String> {
    let pair = cx.pair;
    expect_remote(pair, entry)?;
    if let Some(local) = expect_local(cx, entry)? {
        if !local.is_dir {
            let op = recovery::random_id();
            cx.rec().preserve_local(
                &entry.rel,
                &local,
                &tr::tr!("Replaced by a remote folder"),
                &op,
            )?;
        }
    }
    ensure_local_parent(cx, &entry.rel)?;
    cx.root.mkdir(&entry.rel).map_err(|err| err.to_string())?;
    let local = cx
        .stat_local(&entry.rel)?
        .filter(|local| local.is_dir)
        .ok_or_else(|| tr::tr!("The local folder disappeared after it was created."))?;
    save(pair, &entry.rel, &local, remote, None)
}

fn record(pair: &Pair, entry: &Entry) -> Result<(), String> {
    let (Obs::Present(local), Obs::Present(remote)) = (&entry.local, &entry.remote) else {
        return Err("Both sides must be present to record them as synchronized".into());
    };
    let known = entry.fingerprint.clone().or_else(|| {
        // Contents equal to the baseline keep its fingerprint.
        entry
            .base
            .as_ref()
            .filter(|base| base.verified())
            .and_then(|base| base.fingerprint.clone())
    });
    save(pair, &entry.rel, local, remote, known)
}

/// Keep both versions. Two files: the one with the older modification time
/// (the remote one on a tie) is renamed to a conflict copy on its side, then
/// each version is copied to the other side, so both converge without any
/// action. A file and a folder: the file is renamed and the next pass
/// synchronizes the folder and the copy.
fn keep_both(cx: &Cx, entry: &Entry, kind: ConflictKind) -> Result<(), String> {
    let pair = cx.pair;
    let local = expect_local(cx, entry)?
        .ok_or_else(|| tr::tr!("The local item changed after it was checked."))?;
    let remote = expect_remote(pair, entry)?
        .ok_or_else(|| tr::tr!("The remote item changed after it was checked."))?;
    let op = recovery::random_id();
    let now = observe::Stamp::now();
    let move_local = if local.is_dir != remote.is_dir {
        !local.is_dir
    } else {
        local.mtime < remote.mtime
    };
    let side = if move_local { "local" } else { "remote" };
    let copy = recovery::conflict_rel(&entry.rel, side, &op, now);
    if cx.stat_local(&copy)?.is_some() || pair.stat_remote(&copy)?.is_some() {
        return Err(tr::tr!("A conflict copy with this name already exists."));
    }
    // Journal the rename so a retry never renames the same conflict twice
    // and an interruption still leaves a conflict record.
    let rec = cx.rec();
    rec.begin(
        &op,
        &entry.rel,
        "keep_both",
        serde_json::json!({ "copy": copy, "side": side, "kind": kind_name(kind) }),
    )?;
    let renamed = if move_local {
        cx.root
            .rename_noreplace(&entry.rel, &copy)
            .map_err(|err| tr::tr!("Could not rename the local version [{}].", err))
    } else {
        rclone::sync::move_on_remote(
            &pair.remote.name,
            &pair.remote_path(&entry.rel),
            &pair.remote_path(&copy),
        )
        .map_err(|err| tr::tr!("Could not rename the remote version [{}].", err.error))
    };
    if let Err(err) = renamed {
        rec.abandon(&op, &err);
        return Err(err);
    }
    rec.step(&op, "renamed", None)?;
    failpoint::hit("keep_both:renamed");
    record_conflict(
        cx,
        &entry.rel,
        kind_name(kind),
        &copy,
        side,
        &op,
        Some(&local),
        Some(&remote),
    )?;
    rec.finish(&op)?;
    if local.is_dir != remote.is_dir {
        return Ok(());
    }
    // Copy each version to the side that lacks it now. A failure here is
    // retried by the normal rules: nothing is lost either way.
    if move_local {
        let moved = cx
            .stat_local(&copy)?
            .ok_or_else(|| tr::tr!("The conflict copy disappeared."))?;
        download(
            cx,
            &fresh_entry(&entry.rel, None, Some(remote.clone())),
            &remote,
        )?;
        upload(cx, &fresh_entry(&copy, Some(moved), None), &moved)
    } else {
        let moved = pair
            .stat_remote(&copy)?
            .ok_or_else(|| tr::tr!("The conflict copy disappeared."))?;
        upload(cx, &fresh_entry(&entry.rel, Some(local), None), &local)?;
        download(cx, &fresh_entry(&copy, None, Some(moved.clone())), &moved)
    }
}

fn kind_name(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::Bootstrap => "bootstrap",
        ConflictKind::EditEdit => "edit_edit",
        ConflictKind::FileFolder => "file_folder",
    }
}

/// A remote version that arrived after the check was moved into recovery
/// by our own preservation step: make it visible again as a conflict copy.
fn keep_displaced_remote(
    cx: &Cx,
    rel: &str,
    item: &store::RecoveryItem,
    moved: &RemoteMeta,
    op: &str,
) -> Result<(), String> {
    let pair = cx.pair;
    let copy = recovery::conflict_rel(rel, "remote", op, observe::Stamp::now());
    rclone::sync::copy_on_remote(
        &pair.remote.name,
        &item.stored_path,
        &pair.remote_path(&copy),
    )
    .map_err(|err| err.error)?;
    record_conflict(cx, rel, "edit_edit", &copy, "remote", op, None, Some(moved))
}

/// Record that a deletion met an edit and the edit was restored.
fn note_restore(cx: &Cx, entry: &Entry, restored_on: &str) -> Result<(), String> {
    let op = recovery::random_id();
    let detail = serde_json::json!({
        "rel": entry.rel,
        "restoredOn": restored_on,
        "local": entry.local.present().map(version_json),
        "remote": entry.remote.present().map(remote_json),
    });
    store::insert_conflict(
        cx.pair.db,
        cx.pair.dir.id,
        &cx.internal.pair_uuid,
        &entry.rel,
        "edit_delete",
        &detail.to_string(),
        None,
        &op,
    )
    .map_err(|err| err.to_string())
}

#[allow(clippy::too_many_arguments)]
fn record_conflict(
    cx: &Cx,
    rel: &str,
    kind: &str,
    copy: &str,
    copy_side: &str,
    op: &str,
    local: Option<&LocalMeta>,
    remote: Option<&RemoteMeta>,
) -> Result<(), String> {
    let detail = serde_json::json!({
        "rel": rel,
        "copy": copy,
        "copySide": copy_side,
        "local": local.map(version_json),
        "remote": remote.map(remote_json),
    });
    store::insert_conflict(
        cx.pair.db,
        cx.pair.dir.id,
        &cx.internal.pair_uuid,
        rel,
        kind,
        &detail.to_string(),
        Some(copy),
        op,
    )
    .map_err(|err| err.to_string())
}

fn version_json(meta: &LocalMeta) -> serde_json::Value {
    serde_json::json!({
        "isDir": meta.is_dir,
        "size": meta.size,
        "mtime": meta.mtime.to_string(),
    })
}

fn remote_json(meta: &RemoteMeta) -> serde_json::Value {
    serde_json::json!({
        "isDir": meta.is_dir,
        "size": meta.size,
        "mtime": meta.mtime.to_string(),
    })
}

fn ensure_local_parent(cx: &Cx, rel: &str) -> Result<(), String> {
    let Some((parent, _)) = rel.rsplit_once('/') else {
        return Ok(());
    };
    match cx.stat_local(parent) {
        Ok(Some(meta)) if meta.is_dir => Ok(()),
        Ok(Some(_)) => Err(tr::tr!("The containing folder is not a folder.")),
        Ok(None) => Err(tr::tr!("Waiting for the containing folder to be created.")),
        Err(err) => Err(err),
    }
}

/// Record both sides as holding the same contents.
fn save(
    pair: &Pair,
    rel: &str,
    local: &LocalMeta,
    remote: &RemoteMeta,
    known: Option<Fingerprint>,
) -> Result<(), String> {
    let fingerprint = if local.is_dir {
        None
    } else {
        match known
            .filter(|fp| fp.algo == pair.algo())
            .or_else(|| pair.remote_fingerprint(remote))
        {
            Some(fp) => Some(fp),
            None => Some(pair.hash_local(rel, pair.algo())?),
        }
    };
    let generation = store::next_generation(pair.db, pair.dir.id).map_err(|err| err.to_string())?;
    store::save_baseline(
        pair.db,
        pair.dir,
        rel,
        &BaselineWrite {
            is_dir: local.is_dir,
            size: i64::try_from(local.size).unwrap_or(i64::MAX),
            fingerprint,
            local: *local,
            trust_local: !local.is_racy(observe::Stamp::now()),
            remote_size: remote.size,
            remote_mtime: remote.mtime,
            remote_hash: remote.hash.clone(),
            remote_id: remote.id.clone(),
            generation,
        },
    )
    .map_err(|err| format!("Could not save synchronization history: {err}"))
}

fn blocked_ancestor(entries: &[Entry], rel: &str) -> bool {
    entries
        .iter()
        .any(|entry| entry.skip_children && rel.starts_with(&format!("{}/", entry.rel)))
}

fn retry(pair: &Pair, rel: &str, reason: &str) -> Result<(), String> {
    tracing::info!(rel, "will retry: {reason}");
    store::set_retrying(pair.db, pair.dir.id, rel, reason)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

fn side(obs: &Obs<impl Clone>, kind: Option<bool>, delta: Delta) -> Side {
    match obs {
        Obs::Missing => Side::Missing,
        Obs::Unknown(reason) => Side::Unknown(reason.clone()),
        Obs::Present(_) => Side::Present {
            kind: Kind::of(kind.unwrap_or(false)),
            vs_base: delta,
        },
    }
}

fn decide(pair: &Pair, entry: &mut Entry) {
    let history = match &entry.base {
        None => History::None,
        Some(base) if base.verified() && base.is_dir.is_some() => {
            History::Verified(Kind::of(base.is_dir.unwrap_or(false)))
        }
        Some(_) => History::Unverified,
    };
    let verified = matches!(history, History::Verified(_));
    let (local_delta, local_fp) = match (&entry.local, &entry.base) {
        (Obs::Present(meta), Some(base)) if verified => pair.local_delta(&entry.rel, meta, base),
        _ => (Delta::Changed, None),
    };
    let remote_delta = match (&entry.remote, &entry.base) {
        (Obs::Present(meta), Some(base)) if verified => remote_delta(meta, base),
        _ => Delta::Changed,
    };
    let local_side = side(
        &entry.local,
        entry.local.present().map(|meta| meta.is_dir),
        local_delta,
    );
    let remote_side = side(
        &entry.remote,
        entry.remote.present().map(|meta| meta.is_dir),
        remote_delta,
    );
    let mut input = Input {
        local: local_side,
        remote: remote_side,
        history,
        sides: None,
    };
    entry.fingerprint = local_fp;
    if reconcile::needs_sides(&input.local, &input.remote, history) {
        if let (Obs::Present(local), Obs::Present(remote)) = (&entry.local, &entry.remote) {
            let (delta, fp) = pair.sides_delta(&entry.rel, local, remote);
            input.sides = Some(delta);
            if fp.is_some() {
                entry.fingerprint = fp;
            }
        }
    }
    entry.decision = reconcile::decide(&input);
}

/// A folder is only removed when everything in it is being removed too, and
/// a folder replaced by a file only when nothing in it still matters.
/// Otherwise the folder is kept (restored where it was deleted), or the
/// type change becomes a conflict.
fn settle_folders(entries: &mut [Entry]) {
    for index in (0..entries.len()).rev() {
        let entry = &entries[index];
        let local_dir = entry.local.present().is_some_and(|meta| meta.is_dir);
        let remote_dir = entry.remote.present().is_some_and(|meta| meta.is_dir);
        let clears_local = match entry.decision {
            Decision::DeleteLocal => local_dir,
            Decision::Download => local_dir && !remote_dir,
            _ => false,
        };
        let clears_remote = match entry.decision {
            Decision::DeleteRemote => remote_dir,
            Decision::Upload => remote_dir && !local_dir,
            _ => false,
        };
        if !clears_local && !clears_remote {
            continue;
        }
        let prefix = format!("{}/", entry.rel);
        let mut waiting = false;
        let mut kept = false;
        for child in entries[index + 1..]
            .iter()
            .take_while(|child| child.rel.starts_with(&prefix))
        {
            let removed = if clears_local {
                child.decision.removes_local() || child.local.is_missing()
            } else {
                child.decision.removes_remote() || child.remote.is_missing()
            };
            match &child.decision {
                _ if child.deferred || child.blocked.is_some() => waiting = true,
                Decision::Retry(_) => waiting = true,
                Decision::Retire | Decision::Nothing => {}
                _ if removed => {}
                _ => kept = true,
            }
        }
        let entry = &mut entries[index];
        entry.decision = if waiting {
            Decision::Retry(tr::tr!(
                "Waiting for items inside this folder to be synchronized first."
            ))
        } else if !kept {
            continue;
        } else {
            match entry.decision {
                Decision::DeleteLocal => Decision::RestoreToRemote,
                Decision::DeleteRemote => Decision::RestoreToLocal,
                _ => Decision::KeepBoth(ConflictKind::FileFolder),
            }
        };
    }
}

pub fn load_ignore_globs(ignore_file: &Path) -> Result<Vec<glob::Pattern>, String> {
    match fs::metadata(ignore_file) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.to_string()),
        Ok(metadata) if !metadata.is_file() => {
            return Err("Exclusion rules are not a regular file".into());
        }
        Ok(_) => {}
    }
    let _lock =
        file_lock::FileLock::lock(ignore_file, true, file_lock::FileOptions::new().read(true))
            .map_err(|err| err.to_string())?;
    fs::read_to_string(ignore_file)
        .map_err(|err| err.to_string())?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| glob::Pattern::new(line).map_err(|err| format!("{line}: {err}")))
        .collect()
}

pub fn is_ignored(relative: &str, patterns: &[glob::Pattern]) -> bool {
    if relative == FILE_IGNORE_NAME {
        return true;
    }
    let mut path = relative;
    loop {
        if patterns.iter().any(|pattern| pattern.matches(path)) {
            return true;
        }
        let Some((parent, _)) = path.rsplit_once('/') else {
            return false;
        };
        path = parent;
    }
}

/// The pair's internal folder and recovery store, for actions outside a pass.
fn context<'a>(pair: &'a Pair<'a>) -> Result<Cx<'a>, String> {
    let internal = recovery::ensure_internal(pair.db, pair.dir)?;
    let root = Root::open(Path::new(&pair.dir.local_path), None)
        .map_err(|err| format!("Local sync folder is unavailable: {err}"))?;
    Ok(Cx {
        pair,
        internal,
        root,
        remote_free: Cell::new(None),
    })
}

/// A token for what both sides of `rel` look like now, shown with an action
/// so the action can be refused once either side changes.
pub fn item_token(pair: &Pair, rel: &str) -> Result<String, String> {
    let local = context(pair)?
        .stat_local(rel)?
        .map_or(Obs::Missing, Obs::Present);
    let remote = pair.stat_remote(rel)?.map_or(Obs::Missing, Obs::Present);
    Ok(observe::content_token(&local, &remote))
}

/// Delete a file on both sides after an edit/delete outcome, preserving both
/// versions first. Refused when either side changed since `token`.
pub fn delete_everywhere(pair: &Pair, rel: &str, token: &str) -> Result<(), String> {
    let cx = context(pair)?;
    let local = cx.stat_local(rel)?;
    let remote = pair.stat_remote(rel)?;
    let local_obs = local.map_or(Obs::Missing, Obs::Present);
    let remote_obs = remote.clone().map_or(Obs::Missing, Obs::Present);
    if observe::content_token(&local_obs, &remote_obs) != token {
        return Err(tr::tr!(
            "The item changed after it was shown. Open it again to review the current versions."
        ));
    }
    if local.is_some_and(|meta| meta.is_dir) || remote.as_ref().is_some_and(|meta| meta.is_dir) {
        return Err(tr::tr!("Only files can be deleted this way."));
    }
    let op = recovery::random_id();
    let reason = tr::tr!("Deleted on request after a deletion met an edit");
    if let Some(meta) = &local {
        cx.rec().preserve_local(rel, meta, &reason, &op)?;
    }
    if let Some(meta) = &remote {
        let (item, moved) = cx.rec().preserve_remote(
            rel,
            meta,
            &reason,
            &op,
            &pair.list_options(),
            pair.caps.preferred_hash(),
        )?;
        if !same_remote(meta, &moved) {
            keep_displaced_remote(&cx, rel, &item, &moved, &op)?;
        }
    }
    if let Some(base) = store::load_baselines(pair.db, pair.dir)
        .map_err(|err| err.to_string())?
        .get(rel)
    {
        store::delete_baseline(pair.db, base.id).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// Put a preserved version back on the side it came from. Whatever is at
/// the path now is preserved first; the next pass propagates the restored
/// version like any other change.
pub fn restore(pair: &Pair, id: i64) -> Result<String, String> {
    let cx = context(pair)?;
    let item = store::recovery_by_id(pair.db, &cx.internal.pair_uuid, id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| tr::tr!("This preserved version is no longer available."))?;
    let rel = item.rel.clone();
    let op = recovery::random_id();
    let reason = tr::tr!("Replaced by a restored version");
    if item.side == "local" {
        let source = PathBuf::from(&item.stored_path);
        let source_meta = fs::symlink_metadata(&source)
            .map_err(|_| tr::tr!("This preserved version is no longer available."))?;
        let current = cx.stat_local(&rel)?;
        if current.is_some_and(|meta| meta.is_dir) {
            return Err(tr::tr!("A folder now has this name; rename it first."));
        }
        if let Some((parent, _)) = rel.rsplit_once('/') {
            cx.root.mkdir_all(parent).map_err(|err| err.to_string())?;
        }
        let rec = cx.rec();
        let stage = rec.staging_rel(&op);
        let stage_path = cx.root.path().join(&stage);
        fs::copy(&source, &stage_path).map_err(|err| err.to_string())?;
        fs::File::options()
            .write(true)
            .open(&stage_path)
            .and_then(|file| file.set_modified(source_meta.modified()?))
            .map_err(|err| err.to_string())?;
        let staged = cx
            .stat_local(&stage)?
            .ok_or_else(|| tr::tr!("The restored copy disappeared."))?;
        rec.begin(
            &op,
            &rel,
            "download",
            serde_json::json!({ "stage": stage, "stageInode": staged.inode }),
        )?;
        match current {
            Some(expected) => {
                if let Err(err) = cx.root.exchange(&stage, &rel) {
                    let _ = cx.root.unlink(&stage);
                    rec.abandon(&op, &err.to_string());
                    return Err(err.to_string());
                }
                let displaced = cx
                    .stat_local(&stage)?
                    .ok_or_else(|| tr::tr!("The previous version disappeared."))?;
                if displaced.same_after_rename(&expected) {
                    rec.preserve_path(&stage, &rel, &expected, &reason, &op)?;
                } else {
                    let copy = recovery::conflict_rel(&rel, "local", &op, observe::Stamp::now());
                    cx.root
                        .rename_noreplace(&stage, &copy)
                        .map_err(|err| err.to_string())?;
                }
            }
            None => {
                if let Err(err) = cx.root.rename_noreplace(&stage, &rel) {
                    let _ = cx.root.unlink(&stage);
                    rec.abandon(&op, &err.to_string());
                    return Err(err.to_string());
                }
            }
        }
        rec.finish(&op)?;
    } else {
        let current = pair.stat_remote(&rel)?;
        if current.as_ref().is_some_and(|meta| meta.is_dir) {
            return Err(tr::tr!("A folder now has this name; rename it first."));
        }
        if let Some(expected) = &current {
            cx.rec().preserve_remote(
                &rel,
                expected,
                &reason,
                &op,
                &pair.list_options(),
                pair.caps.preferred_hash(),
            )?;
        }
        rclone::sync::copy_on_remote(
            &pair.remote.name,
            &item.stored_path,
            &pair.remote_path(&rel),
        )
        .map_err(|err| err.error)?;
    }
    store::mark_recovery(pair.db, item.id, "restored_at").map_err(|err| err.to_string())?;
    Ok(rel)
}

/// Preserved versions to offer for restoring, newest first.
pub fn recovery_list(pair: &Pair) -> Result<Vec<store::RecoveryItem>, String> {
    let uuid = store::pair_uuid(pair.db, pair.dir.id).map_err(|err| err.to_string())?;
    store::list_recovery(pair.db, &uuid).map_err(|err| err.to_string())
}
