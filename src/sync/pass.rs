//! One synchronization pass over a pair.
//!
//! A pass observes the whole local tree, lists the remote folder once (with
//! provider hashes) and reads the pair's history once. Every item then goes
//! through the single decision in [`super::reconcile`]. One item failing or
//! conflicting never stops the others: it is retried with backoff or blocked
//! with a visible reason. Only problems with the whole pair (an unavailable
//! or emptied root, unreadable exclusions, a database failure) stop a pass.

use super::capabilities::Capabilities;
use super::model::{FILE_IGNORE_NAME, SyncError};
use super::observe::{self, Fingerprint, HashError, LocalMeta, LocalTree, Obs, RemoteMeta};
use super::reconcile::{self, ConflictKind, Decision, Delta, History, Input, Kind, Side};
use super::store::{self, Baseline, BaselineWrite};
use crate::entities::{RemotesModel, SyncDirsModel};
use crate::rclone::{self, ListOptions, RcloneListFilter};
use sea_orm::DatabaseConnection;
use std::collections::{BTreeMap, BTreeSet};
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

    fn stat_local(&self, rel: &str) -> Result<Option<LocalMeta>, String> {
        let path = self.local_path(rel);
        validate_local_path(Path::new(&self.dir.local_path), &path)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                Err("Symbolic links inside a synced folder are not followed".into())
            }
            Ok(metadata) => Ok(Some(LocalMeta::from_metadata(&metadata))),
            Err(err) if missing(&err) => Ok(None),
            Err(err) => Err(err.to_string()),
        }
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

/// Run one pass. `cached` is a local observation the watcher vouches for.
pub fn run(
    pair: &Pair,
    hooks: &mut dyn Hooks,
    cached: Option<LocalTree>,
) -> Result<Report, String> {
    let root = Path::new(&pair.dir.local_path);
    let patterns = load_ignore_globs(&root.join(FILE_IGNORE_NAME))?;
    let ignored = |rel: &str| is_ignored(rel, &patterns);
    let local = match cached {
        Some(tree) => tree,
        None => observe::observe_local(root, &|rel, _| ignored(rel))
            .map_err(|err| format!("Local sync folder is unavailable: {err}"))?,
    };
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
        &ignored,
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
    for rel in rels {
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
                execute_removal(pair, entry, step).map(|()| report.actions += 1)
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
        if failed[index] || entry.deferred {
            continue;
        }
        if let Some(reason) = &entry.blocked {
            store::set_blocked(pair.db, pair.dir.id, &entry.rel, reason)
                .map_err(|err| err.to_string())?;
            continue;
        }
        let outcome = match &entry.decision {
            Decision::Retry(reason) => Err(reason.clone()),
            Decision::KeepBoth(kind) => {
                store::set_blocked(pair.db, pair.dir.id, &entry.rel, &conflict_reason(*kind))
                    .map_err(|err| err.to_string())?;
                continue;
            }
            Decision::InSync | Decision::Nothing => Ok(false),
            Decision::DeleteLocal | Decision::DeleteRemote => Ok(false),
            decision => {
                hooks.progress(tr::tr!("Synchronizing '{}'...", entry.rel));
                execute(pair, entry, decision).map(|()| true)
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
    report.local_tree = Some(local);
    Ok(report)
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

fn conflict_reason(kind: ConflictKind) -> String {
    match kind {
        ConflictKind::FileFolder => {
            tr::tr!("A file and a folder share this name. Rename or remove one of them.")
        }
        ConflictKind::Bootstrap => tr::tr!(
            "The local and remote contents differ and there is no common earlier version. Both are kept unchanged."
        ),
        ConflictKind::EditEdit => tr::tr!(
            "Both the local and remote item changed since the last sync. Both are kept unchanged."
        ),
    }
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

#[derive(Clone, Copy)]
enum Removal {
    LocalFile,
    LocalDir,
    RemoteFile,
    RemoteDir,
}

/// The removal an entry needs before anything is created.
fn removal(entry: &Entry) -> Option<Removal> {
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

fn execute_removal(pair: &Pair, entry: &Entry, step: Removal) -> Result<(), String> {
    let deletion = matches!(
        entry.decision,
        Decision::DeleteLocal | Decision::DeleteRemote
    );
    match step {
        Removal::LocalFile | Removal::LocalDir => {
            let Obs::Present(expected) = &entry.local else {
                return Err("The local item is no longer present".into());
            };
            // Observe both sides again immediately before deleting.
            if !pair
                .stat_local(&entry.rel)?
                .is_some_and(|now| now.same_tuple(expected))
            {
                return Err(tr::tr!("The local item changed after it was checked."));
            }
            if deletion && pair.stat_remote(&entry.rel)?.is_some() {
                return Err(tr::tr!("The remote item reappeared after it was checked."));
            }
            let path = pair.local_path(&entry.rel);
            if matches!(step, Removal::LocalDir) {
                fs::remove_dir(&path).map_err(|err| {
                    tr::tr!(
                        "The folder still has contents that are not being deleted ({}).",
                        err
                    )
                })?;
            } else {
                fs::remove_file(&path).map_err(|err| err.to_string())?;
            }
        }
        Removal::RemoteFile | Removal::RemoteDir => {
            let Obs::Present(expected) = &entry.remote else {
                return Err("The remote item is no longer present".into());
            };
            if !pair
                .stat_remote(&entry.rel)?
                .is_some_and(|now| same_remote(expected, &now))
            {
                return Err(tr::tr!("The remote item changed after it was checked."));
            }
            if deletion && pair.stat_local(&entry.rel)?.is_some() {
                return Err(tr::tr!("The local item reappeared after it was checked."));
            }
            let path = pair.remote_path(&entry.rel);
            if matches!(step, Removal::RemoteDir) {
                rclone::sync::rmdir(&pair.remote.name, &path).map_err(|err| {
                    tr::tr!(
                        "The remote folder still has contents that are not being deleted ({}).",
                        err.error
                    )
                })?;
            } else {
                rclone::sync::delete(&pair.remote.name, &path).map_err(|err| err.error)?;
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

fn execute(pair: &Pair, entry: &Entry, decision: &Decision) -> Result<(), String> {
    match decision {
        Decision::Upload | Decision::RestoreToRemote => {
            let Obs::Present(local) = &entry.local else {
                return Err("The local item is no longer present".into());
            };
            if local.is_dir {
                create_remote_dir(pair, entry, local)
            } else {
                upload(pair, entry, local)
            }
        }
        Decision::Download | Decision::RestoreToLocal => {
            let Obs::Present(remote) = &entry.remote else {
                return Err("The remote item is no longer present".into());
            };
            if remote.is_dir {
                create_local_dir(pair, entry, remote)
            } else {
                download(pair, entry, remote)
            }
        }
        Decision::Record => record(pair, entry),
        Decision::Retire => match &entry.base {
            Some(base) => store::delete_baseline(pair.db, base.id).map_err(|err| err.to_string()),
            None => Ok(()),
        },
        _ => Ok(()),
    }
}

/// The local side must still be what was observed.
fn expect_local(pair: &Pair, entry: &Entry) -> Result<Option<LocalMeta>, String> {
    let now = pair.stat_local(&entry.rel)?;
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

/// Folder times change whenever their contents do; a folder is compared by
/// kind only, a file by everything the listing reports.
fn same_remote(seen: &RemoteMeta, now: &RemoteMeta) -> bool {
    if seen.is_dir || now.is_dir {
        seen.is_dir == now.is_dir
    } else {
        seen == now
    }
}

/// A path is absent when it, or one of its parents, does not exist.
fn missing(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn upload(pair: &Pair, entry: &Entry, local: &LocalMeta) -> Result<(), String> {
    expect_local(pair, entry)?;
    if entry.remote.present().is_some_and(|remote| remote.is_dir) {
        // The folder this file replaces was emptied and removed first.
        if pair.stat_remote(&entry.rel)?.is_some() {
            return Err(tr::tr!("The remote folder could not be replaced yet."));
        }
    } else {
        expect_remote(pair, entry)?;
    }
    let source = pair.local_path(&entry.rel);
    let source = source.to_str().ok_or(observe::NON_UTF8)?;
    rclone::sync::copy_to_remote(source, &pair.remote.name, &pair.remote_path(&entry.rel))
        .map_err(|err| err.error)?;
    // Acknowledge only the bytes that were read: if the file changed while
    // it was uploaded, the next pass uploads it again.
    let after = pair
        .stat_local(&entry.rel)?
        .filter(|after| after.same_tuple(local))
        .ok_or_else(|| tr::tr!("The local file changed while it was uploaded."))?;
    let remote = pair
        .stat_remote(&entry.rel)?
        .ok_or_else(|| tr::tr!("The remote item disappeared after it was uploaded."))?;
    save(pair, &entry.rel, &after, &remote, None)
}

fn download(pair: &Pair, entry: &Entry, remote: &RemoteMeta) -> Result<(), String> {
    if entry.local.present().is_some_and(|local| local.is_dir) {
        if pair.stat_local(&entry.rel)?.is_some() {
            return Err(tr::tr!("The local folder could not be replaced yet."));
        }
    } else {
        expect_local(pair, entry)?;
    }
    expect_remote(pair, entry)?;
    let path = pair.local_path(&entry.rel);
    ensure_local_parent(pair, &path)?;
    let target = path.to_str().ok_or(observe::NON_UTF8)?;
    rclone::sync::copy_to_local(target, &pair.remote.name, &pair.remote_path(&entry.rel))
        .map_err(|err| err.error)?;
    let after_remote = pair.stat_remote(&entry.rel)?;
    if after_remote.as_ref() != Some(remote) {
        return Err(tr::tr!("The remote file changed while it was downloaded."));
    }
    let local = pair
        .stat_local(&entry.rel)?
        .ok_or_else(|| tr::tr!("The local item disappeared after it was downloaded."))?;
    save(pair, &entry.rel, &local, remote, None)
}

fn create_remote_dir(pair: &Pair, entry: &Entry, local: &LocalMeta) -> Result<(), String> {
    expect_local(pair, entry)?;
    let path = pair.remote_path(&entry.rel);
    match expect_remote(pair, entry)? {
        Some(remote) if !remote.is_dir => {
            rclone::sync::delete(&pair.remote.name, &path).map_err(|err| err.error)?;
        }
        _ => {}
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

fn create_local_dir(pair: &Pair, entry: &Entry, remote: &RemoteMeta) -> Result<(), String> {
    expect_remote(pair, entry)?;
    let path = pair.local_path(&entry.rel);
    match expect_local(pair, entry)? {
        Some(local) if !local.is_dir => fs::remove_file(&path).map_err(|err| err.to_string())?,
        _ => {}
    }
    ensure_local_parent(pair, &path)?;
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err.to_string()),
    }
    let local = pair
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

fn ensure_local_parent(pair: &Pair, path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid local path")?;
    validate_local_path(Path::new(&pair.dir.local_path), parent)?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(tr::tr!("The containing folder is not a folder.")),
        Err(_) => Err(tr::tr!("Waiting for the containing folder to be created.")),
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

pub fn validate_local_path(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path.strip_prefix(root).map_err(|err| err.to_string())?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Symbolic links inside a synced folder are not followed".into());
            }
            Ok(_) => {}
            Err(err) if missing(&err) => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}
