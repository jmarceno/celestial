//! Preserved versions: the pair's internal folder, recovery copies of every
//! version Celestial replaces or deletes, conflict-copy names and retention.
//!
//! Local recovery lives in an internal folder that Celestial creates inside
//! the pair root (same filesystem, so preservation is a rename). Remote
//! recovery lives in `.celestial-recovery/<pair>` at the root of the remote,
//! outside the synced folder unless the whole remote is synced, and is
//! visible to other clients of the account. Provider trash is never relied
//! on.

use super::failpoint;
use super::localfs::Root;
use super::observe::{LocalMeta, RemoteMeta, Stamp};
use super::store::{self, RecoveryItem};
use crate::entities::{RemotesModel, SyncDirsModel};
use crate::rclone;
use sea_orm::DatabaseConnection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Remote folder (at the remote root) holding preserved remote versions.
pub const REMOTE_RECOVERY_ROOT: &str = ".celestial-recovery";
const MARKER: &str = ".celestial-owner";
/// Preserved versions kept per item, and for how long.
pub const KEEP_VERSIONS: usize = 10;
/// Visible conflict copies kept per item.
pub const MAX_CONFLICT_COPIES: usize = 10;

/// A random identifier for operations.
pub fn random_id() -> String {
    let mut bytes = [0u8; 8];
    let filled = fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if filled.is_err() {
        // Fall back to the clock; uniqueness is still checked by every
        // no-replace creation that uses the identifier.
        let now = Stamp::now();
        bytes = (now.total_nanos() as u64).to_le_bytes();
    }
    bytes.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The pair's internal folder.
#[derive(Clone, Debug)]
pub struct Internal {
    pub name: String,
    pub path: PathBuf,
    pub identity: (u64, u64),
    pub pair_uuid: String,
}

impl Internal {
    pub fn staging(&self) -> PathBuf {
        self.path.join("staging")
    }

    pub fn recovery(&self) -> PathBuf {
        self.path.join("recovery")
    }

    /// Whether a local item is the internal folder itself.
    pub fn is(&self, metadata: &fs::Metadata) -> bool {
        (metadata.dev(), metadata.ino()) == self.identity
    }
}

/// Find or create the pair's internal folder. It is never a folder the
/// person created: an existing folder is only used when it carries this
/// pair's marker and the identity recorded when Celestial created it.
pub fn ensure_internal(db: &DatabaseConnection, dir: &SyncDirsModel) -> Result<Internal, String> {
    let pair_uuid = store::pair_uuid(db, dir.id).map_err(|err| err.to_string())?;
    let root = Path::new(&dir.local_path);
    let root_dev = fs::metadata(root).map_err(|err| err.to_string())?.dev();
    let recorded = store::internal_dir(db, dir.id).map_err(|err| err.to_string())?;
    if let Some((name, identity)) = &recorded {
        let path = root.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.is_dir()
                && metadata.dev() == root_dev
                && fs::read_to_string(path.join(MARKER)).ok().as_deref() == Some(&pair_uuid)
            {
                // The marker holds this pair's random identity, so the folder
                // is ours even when copying or restoring the synced folder
                // gave it a new inode; it is adopted rather than left to be
                // synchronized as ordinary contents.
                let current = (metadata.dev(), metadata.ino());
                if current != *identity {
                    store::set_internal_dir(db, dir.id, name, current)
                        .map_err(|err| err.to_string())?;
                    tracing::info!(path = %path.display(), "internal folder has a new identity; adopted by its marker");
                }
                let internal = Internal {
                    name: name.clone(),
                    path,
                    identity: current,
                    pair_uuid,
                };
                create_subfolders(&internal)?;
                return Ok(internal);
            }
        }
    }
    for attempt in 0..100 {
        let name = if attempt == 0 {
            format!(".celestial-{}", &pair_uuid[..8.min(pair_uuid.len())])
        } else {
            format!(
                ".celestial-{}-{attempt}",
                &pair_uuid[..8.min(pair_uuid.len())]
            )
        };
        let path = root.join(&name);
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(tr::tr!(
                    "Could not create Celestial's internal folder in the sync folder [{}].",
                    err
                ));
            }
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .map_err(|err| err.to_string())?;
        fs::write(path.join(MARKER), &pair_uuid).map_err(|err| err.to_string())?;
        let metadata = fs::symlink_metadata(&path).map_err(|err| err.to_string())?;
        if metadata.dev() != root_dev {
            return Err(tr::tr!(
                "Celestial's internal folder is not on the same filesystem as the sync folder."
            ));
        }
        let internal = Internal {
            name,
            path,
            identity: (metadata.dev(), metadata.ino()),
            pair_uuid,
        };
        create_subfolders(&internal)?;
        store::set_internal_dir(db, dir.id, &internal.name, internal.identity)
            .map_err(|err| err.to_string())?;
        tracing::info!(path = %internal.path.display(), "created internal folder");
        return Ok(internal);
    }
    Err(tr::tr!(
        "Could not find a free name for Celestial's internal folder."
    ))
}

/// Whether a local folder is an internal folder of any pair: this pair's
/// before it was adopted, a stopped pair's, or a nested pair's. They carry
/// Celestial's ownership marker and hold preserved versions, so they are
/// never synchronized.
pub fn is_marked_internal(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.is_dir()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".celestial-"))
        && fs::symlink_metadata(path.join(MARKER)).is_ok_and(|marker| marker.is_file())
}

fn create_subfolders(internal: &Internal) -> Result<(), String> {
    for path in [internal.staging(), internal.recovery()] {
        match fs::create_dir(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

/// `<stem>.sync-conflict-<yyyymmdd-hhmmss>-<side>-<op>.<ext>` in the same
/// folder as `rel`, shortened to fit a 255-byte file name.
pub fn conflict_rel(rel: &str, side: &str, op: &str, now: Stamp) -> String {
    let (parent, name) = match rel.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, rel),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    };
    let stamp = now
        .to_datetime()
        .map(|time| {
            format!(
                "{:04}{:02}{:02}-{:02}{:02}{:02}",
                time.year(),
                u8::from(time.month()),
                time.day(),
                time.hour(),
                time.minute(),
                time.second()
            )
        })
        .unwrap_or_else(|| "00000000-000000".into());
    let tag = format!(".sync-conflict-{stamp}-{side}-{}", &op[..8.min(op.len())]);
    let mut stem = stem.to_owned();
    while stem.len() + tag.len() + ext.len() > 255 && !stem.is_empty() {
        stem.pop();
    }
    let file = format!("{stem}{tag}{ext}");
    match parent {
        Some(parent) => format!("{parent}/{file}"),
        None => file,
    }
}

/// The item a conflict copy belongs to, and the copy's timestamp text.
pub fn conflict_origin(rel: &str) -> Option<(String, String)> {
    let (parent, name) = match rel.rsplit_once('/') {
        Some((parent, name)) => (Some(parent), name),
        None => (None, rel),
    };
    let index = name.find(".sync-conflict-")?;
    let rest = &name[index + ".sync-conflict-".len()..];
    let stamp = rest.get(..15)?;
    let after = rest.get(15..)?;
    let after = after
        .strip_prefix("-local-")
        .or_else(|| after.strip_prefix("-remote-"))?;
    let ext = after.get(8..)?;
    if !stamp.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return None;
    }
    let file = format!("{}{ext}", &name[..index]);
    let origin = match parent {
        Some(parent) => format!("{parent}/{file}"),
        None => file,
    };
    Some((origin, stamp.to_owned()))
}

/// Where everything for one pair lives.
pub struct Store<'a> {
    pub db: &'a DatabaseConnection,
    pub remote: &'a RemotesModel,
    pub dir: &'a SyncDirsModel,
    pub internal: &'a Internal,
    pub root: &'a Root,
}

impl Store<'_> {
    /// Whether this pair keeps a preserved copy of a file of `size` bytes.
    /// A bigger file is replaced or deleted without one, so a large folder
    /// does not fill the recovery store with versions of its biggest file;
    /// a limit of `0` keeps no copies at all.
    pub fn keeps_version(&self, size: u64) -> bool {
        let limit = self.dir.recovery_max_bytes;
        limit > 0 && i64::try_from(size).is_ok_and(|size| size <= limit)
    }

    fn version_folder(&self, op: &str) -> String {
        format!("{}-{}", Stamp::now().secs, op)
    }

    /// The staging path of an operation, relative to the pair root.
    pub fn staging_rel(&self, op: &str) -> String {
        format!("{}/staging/{op}", self.internal.name)
    }

    /// The remote folder that keeps this pair's preserved versions.
    pub fn remote_root(&self) -> String {
        format!("{REMOTE_RECOVERY_ROOT}/{}", self.internal.pair_uuid)
    }

    fn absolute(&self, rel: &str) -> String {
        self.root.path().join(rel).display().to_string()
    }

    // ------------------------------------------------------------ journal

    /// Record an operation before it changes anything.
    pub fn begin(&self, op: &str, rel: &str, action: &str, payload: Value) -> Result<(), String> {
        self.begin_at(op, rel, action, "planned", payload)
    }

    /// Open the journal entry of an operation directly at `state`, for one
    /// whose earlier steps leave nothing to recover.
    pub fn begin_at(
        &self,
        op: &str,
        rel: &str,
        action: &str,
        state: &str,
        payload: Value,
    ) -> Result<(), String> {
        store::insert_operation(
            self.db,
            &store::Operation {
                id: op.to_owned(),
                sync_dir_id: self.dir.id,
                pair_uuid: self.internal.pair_uuid.clone(),
                rel: rel.to_owned(),
                action: action.to_owned(),
                state: state.into(),
                payload,
            },
        )
        .map_err(|err| format!("Could not record the operation: {err}"))
    }

    pub fn step(&self, op: &str, state: &str, payload: Option<&Value>) -> Result<(), String> {
        store::update_operation(self.db, op, state, payload, None)
            .map_err(|err| format!("Could not record the operation: {err}"))
    }

    /// Close the journal entry. This records the outcome, so it commits with
    /// the rest of it (see [`store::outcome`]).
    pub fn finish(&self, op: &str) -> Result<(), String> {
        let op = op.to_owned();
        store::outcome(self.db, move |tx| {
            store::update_operation(tx, &op, "complete", None, None)
        })
        .map_err(|err| format!("Could not record the operation: {err}"))
    }

    /// Mark an operation that stopped before changing anything durable.
    pub fn abandon(&self, op: &str, error: &str) {
        if let Err(err) = store::update_operation(self.db, op, "failed", None, Some(error)) {
            tracing::warn!("could not record a failed operation: {err}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        rel: &str,
        side: &str,
        stored: String,
        reason: &str,
        op: &str,
        size: Option<i64>,
        mtime: Option<i64>,
    ) -> Result<RecoveryItem, String> {
        let item = RecoveryItem {
            id: 0,
            pair_uuid: self.internal.pair_uuid.clone(),
            remote_name: self.remote.name.clone(),
            local_root: self.dir.local_path.clone(),
            remote_root: self.dir.remote_path.clone(),
            rel: rel.to_owned(),
            side: side.to_owned(),
            stored_path: stored,
            reason: reason.to_owned(),
            op_id: op.to_owned(),
            size,
            mtime,
            created_at: store::now(),
        };
        store::insert_recovery(self.db, &item)
            .map_err(|err| format!("Could not record a preserved version: {err}"))?;
        Ok(item)
    }

    /// Move a local file into the recovery folder. The displaced inode is
    /// compared with the one expected; a different file is put back.
    pub fn preserve_local(
        &self,
        rel: &str,
        expected: &LocalMeta,
        reason: &str,
        op: &str,
    ) -> Result<RecoveryItem, String> {
        self.preserve_path(rel, rel, expected, reason, op)
    }

    /// Preserve the file at `source` (relative to the pair root) as a version
    /// of `rel`. The move is journaled first, so an interruption at any point
    /// leaves a version that the replay records rather than loses.
    pub fn preserve_path(
        &self,
        source: &str,
        rel: &str,
        expected: &LocalMeta,
        reason: &str,
        op: &str,
    ) -> Result<RecoveryItem, String> {
        let target = format!(
            "{}/recovery/{}/{rel}",
            self.internal.name,
            self.version_folder(op)
        );
        let current = self
            .root
            .lstat(source)
            .map_err(|err| err.to_string())?
            .ok_or_else(|| tr::tr!("The local item changed after it was checked."))?;
        if current.device != self.internal.identity.0 {
            return Err(tr::tr!(
                "This item is on another filesystem than the sync folder; it is paused so it can be preserved before it is replaced."
            ));
        }
        if let Some((parent, _)) = target.rsplit_once('/') {
            self.root.mkdir_all(parent).map_err(|err| err.to_string())?;
        }
        let journal_id = format!("{op}-keep-{}", random_id());
        self.begin(
            &journal_id,
            rel,
            "preserve_local",
            json!({ "source": source, "target": target, "reason": reason, "op": op }),
        )?;
        failpoint::hit("preserve_local:before_move");
        if let Err(err) = self.root.rename_noreplace(source, &target) {
            self.abandon(&journal_id, &err.to_string());
            return Err(tr::tr!(
                "Could not move the previous version aside [{}].",
                err
            ));
        }
        failpoint::hit("preserve_local:after_move");
        let moved = self.root.lstat(&target).ok().flatten();
        if !moved.is_some_and(|moved| moved.same_after_rename(expected)) {
            // Something else was at the path by then: leave it where it was.
            let _ = self.root.rename_noreplace(&target, source);
            self.abandon(&journal_id, "changed before it was preserved");
            return Err(tr::tr!("The local item changed after it was checked."));
        }
        let item = self.record(
            rel,
            "local",
            self.absolute(&target),
            reason,
            op,
            Some(i64::try_from(expected.size).unwrap_or(i64::MAX)),
            Some(expected.mtime.secs),
        )?;
        self.finish(&journal_id)?;
        Ok(item)
    }

    /// Move a remote file into the remote recovery folder (server-side when
    /// the backend can move). Returns what was actually moved; when it is
    /// not what was expected, an edit arrived after the check, and the
    /// caller keeps it as a conflict copy.
    pub fn preserve_remote(
        &self,
        rel: &str,
        expected: &RemoteMeta,
        reason: &str,
        op: &str,
        hash_opts: &rclone::ListOptions,
        hash_type: Option<&str>,
    ) -> Result<(RecoveryItem, RemoteMeta), String> {
        let source = store::item_paths(self.dir, rel).1;
        let target = format!("{}/{}/{rel}", self.remote_root(), self.version_folder(op));
        let journal_id = format!("{op}-keep-{}", random_id());
        self.begin(
            &journal_id,
            rel,
            "preserve_remote",
            json!({ "source": source, "target": target, "reason": reason, "op": op }),
        )?;
        failpoint::hit("preserve_remote:before_move");
        if let Err(err) = rclone::sync::move_on_remote(&self.remote.name, &source, &target) {
            self.abandon(&journal_id, &err.error);
            return Err(tr::tr!(
                "Could not move the previous version aside [{}].",
                err
            ));
        }
        failpoint::hit("preserve_remote:after_move");
        let moved = rclone::sync::stat_with(&self.remote.name, &target, hash_opts)
            .map_err(|err| err.error)?
            .map(|item| RemoteMeta::from_item(&item, hash_type))
            .ok_or_else(|| tr::tr!("The preserved remote version could not be found."))?;
        let item = self.record(
            rel,
            "remote",
            target,
            reason,
            op,
            Some(moved.size),
            Some(moved.mtime.secs),
        )?;
        self.finish(&journal_id)?;
        if &moved != expected {
            tracing::warn!(rel, "remote item changed before it was preserved");
        }
        Ok((item, moved))
    }

    // ------------------------------------------------------------- replay

    /// Finish or undo operations interrupted by a crash, a kill or a failed
    /// database write. Every step is idempotent. Anything that cannot be
    /// proven safe stays journaled and is reported.
    pub fn replay(&self) -> Result<usize, String> {
        let mut ops = store::incomplete_operations(self.db, Some(self.dir.id))
            .map_err(|err| err.to_string())?;
        // Record interrupted preservations first, so the operations that
        // started them can find what was preserved.
        ops.sort_by_key(|op| !op.action.starts_with("preserve_"));
        let mut replayed = 0;
        for op in &ops {
            let text = |key: &str| op.payload[key].as_str().unwrap_or_default().to_owned();
            match op.action.as_str() {
                "preserve_local" => {
                    let target = text("target");
                    let recorded =
                        store::recovery_for_op(self.db, &text("op"), &self.absolute(&target))
                            .map_err(|err| err.to_string())?;
                    match self.root.lstat(&target).map_err(|err| err.to_string())? {
                        Some(meta) if !recorded => {
                            self.record(
                                &op.rel,
                                "local",
                                self.absolute(&target),
                                &text("reason"),
                                &text("op"),
                                Some(i64::try_from(meta.size).unwrap_or(i64::MAX)),
                                Some(meta.mtime.secs),
                            )?;
                            self.finish(&op.id)?;
                        }
                        Some(_) => self.finish(&op.id)?,
                        None => self.abandon(&op.id, "interrupted before the move"),
                    }
                }
                "preserve_remote" => {
                    let target = text("target");
                    let recorded = store::recovery_for_op(self.db, &text("op"), &target)
                        .map_err(|err| err.to_string())?;
                    match rclone::sync::stat(&self.remote.name, &target).map_err(|err| err.error)? {
                        Some(item) if !recorded => {
                            self.record(
                                &op.rel,
                                "remote",
                                target,
                                &text("reason"),
                                &text("op"),
                                Some(item.size),
                                Some(item.mod_time.unix_timestamp()),
                            )?;
                            self.finish(&op.id)?;
                        }
                        Some(_) => self.finish(&op.id)?,
                        None => self.abandon(&op.id, "interrupted before the move"),
                    }
                }
                "download" => {
                    let stage = text("stage");
                    let staged_inode = op.payload["stageInode"].as_u64();
                    let dest = self.root.lstat(&op.rel).map_err(|err| err.to_string())?;
                    let committed = staged_inode.is_some()
                        && dest.is_some_and(|dest| Some(dest.inode) == staged_inode);
                    match self.root.lstat(&stage).map_err(|err| err.to_string())? {
                        // After an exchange the stage holds the displaced
                        // version: preserve it.
                        Some(displaced) if committed => {
                            if self.keeps_version(displaced.size) {
                                self.preserve_path(
                                    &stage,
                                    &op.rel,
                                    &displaced,
                                    &tr::tr!("Replaced by the remote version"),
                                    &op.id,
                                )?;
                            } else {
                                // Bigger than this pair keeps a copy of.
                                let _ = self.root.unlink(&stage);
                            }
                        }
                        // The download never reached the item: discard it.
                        Some(_) => {
                            let _ = self.root.unlink(&stage);
                        }
                        None => {}
                    }
                    self.abandon(&op.id, "interrupted; the item is checked again");
                }
                "upload" => {
                    // The step after preserving may not have been journaled:
                    // the inventory knows what this operation preserved.
                    let mut backup = text("backup");
                    if backup.is_empty() {
                        backup = store::recovery_of_op(self.db, &op.id, "remote", &op.rel)
                            .map_err(|err| err.to_string())?
                            .unwrap_or_default();
                    }
                    let destination = text("remote");
                    if !backup.is_empty()
                        && matches!(
                            rclone::sync::stat(&self.remote.name, &destination),
                            Ok(None)
                        )
                    {
                        // The previous version was moved aside but nothing
                        // replaced it: put it back.
                        rclone::sync::move_on_remote(&self.remote.name, &backup, &destination)
                            .map_err(|err| err.error)?;
                        store::forget_recovery(self.db, &op.id, &backup)
                            .map_err(|err| err.to_string())?;
                    }
                    let snapshot = text("snapshot");
                    if !snapshot.is_empty() {
                        let _ = self.root.unlink(&snapshot);
                    }
                    self.abandon(&op.id, "interrupted; the item is checked again");
                }
                "keep_both" => {
                    // The copy's name carries the operation ID, so finding
                    // it proves the rename happened even if the step after
                    // it was not journaled.
                    let copy = text("copy");
                    let renamed = op.state == "renamed"
                        || if text("side") == "local" {
                            self.root
                                .lstat(&copy)
                                .map_err(|err| err.to_string())?
                                .is_some()
                        } else {
                            rclone::sync::stat(
                                &self.remote.name,
                                &store::item_paths(self.dir, &copy).1,
                            )
                            .map_err(|err| err.error)?
                            .is_some()
                        };
                    if renamed {
                        // The history described the version that was renamed.
                        if let Some(base) = store::load_baselines(self.db, self.dir)
                            .map_err(|err| err.to_string())?
                            .get(&op.rel)
                        {
                            store::delete_baseline(self.db, base.id)
                                .map_err(|err| err.to_string())?;
                        }
                    }
                    if renamed
                        && !store::conflict_for_op(self.db, &op.id)
                            .map_err(|err| err.to_string())?
                    {
                        store::insert_conflict(
                            self.db,
                            self.dir.id,
                            &self.internal.pair_uuid,
                            &op.rel,
                            &text("kind"),
                            &json!({ "rel": op.rel, "copy": text("copy"), "copySide": text("side") })
                                .to_string(),
                            Some(&text("copy")),
                            &op.id,
                        )
                        .map_err(|err| err.to_string())?;
                    }
                    if renamed {
                        self.finish(&op.id)?;
                    } else {
                        self.abandon(&op.id, "interrupted before the rename");
                    }
                }
                other => {
                    return Err(tr::tr!(
                        "An interrupted operation of an unknown kind ({}) needs attention.",
                        other
                    ));
                }
            }
            replayed += 1;
        }
        // Every interrupted operation is resolved, so whatever is left in
        // staging (including rclone's own partial files) is a download that
        // never replaced anything: it is disposable.
        if let Ok(entries) = fs::read_dir(self.internal.staging()) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        if replayed > 0 {
            tracing::info!(replayed, "recovered interrupted operations");
        }
        Ok(replayed)
    }

    /// Delete preserved versions beyond the retention policy: the newest
    /// versions of each item are kept for the pair's chosen number of days
    /// (the only copy of an item that no longer exists anywhere too), but
    /// those of unresolved conflicts or incomplete operations are never
    /// deleted.
    pub fn prune(&self, live: &HashSet<String>) -> Result<usize, String> {
        let items = store::list_recovery(self.db, &self.internal.pair_uuid)
            .map_err(|err| err.to_string())?;
        let protected = store::protected_operations(self.db, &self.internal.pair_uuid)
            .map_err(|err| err.to_string())?;
        let mut by_item: BTreeMap<&str, Vec<&RecoveryItem>> = BTreeMap::new();
        for item in &items {
            by_item.entry(item.rel.as_str()).or_default().push(item);
        }
        let days =
            store::keep_versions_days(self.db, self.dir.id).map_err(|err| err.to_string())?;
        let now = store::now();
        let mut pruned = 0;
        for (rel, versions) in by_item {
            // Already sorted newest first.
            for (index, item) in versions.iter().enumerate() {
                let fresh = days == 0 || now - item.created_at < days * 24 * 3600;
                let only_copy = index == 0 && !live.contains(rel);
                if (fresh && (index < KEEP_VERSIONS || only_copy))
                    || protected.contains(&item.op_id)
                {
                    continue;
                }
                let removed = self.remove_stored(item);
                match removed {
                    Ok(()) => {
                        store::mark_recovery(self.db, item.id, "pruned_at")
                            .map_err(|err| err.to_string())?;
                        pruned += 1;
                    }
                    Err(err) => tracing::warn!(rel, "could not prune a preserved version: {err}"),
                }
            }
        }
        if pruned > 0 {
            remove_empty_dirs(&self.internal.recovery());
        }
        Ok(pruned)
    }

    /// Delete a preserved version's stored copy; a copy already gone counts
    /// as deleted.
    fn remove_stored(&self, item: &RecoveryItem) -> Result<(), String> {
        if item.side == "local" {
            match fs::remove_file(&item.stored_path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err.to_string()),
            }
        } else {
            match rclone::sync::delete(&self.remote.name, &item.stored_path) {
                Ok(()) => Ok(()),
                Err(err) => {
                    match rclone::sync::stat(&self.remote.name, &item.stored_path) {
                        Ok(None) => Ok(()),
                        _ => Err(err.error),
                    }
                }
            }
        }
    }

    /// Delete one preserved version the user no longer wants.
    pub fn discard(&self, item: &RecoveryItem) -> Result<(), String> {
        self.remove_stored(item)?;
        store::mark_recovery(self.db, item.id, "pruned_at").map_err(|err| err.to_string())?;
        remove_empty_dirs(&self.internal.recovery());
        Ok(())
    }
}

/// Remove empty folders left under `root` (not `root` itself).
pub fn remove_empty_dirs(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_empty_dirs(&path);
            let _ = fs::remove_dir(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_names_keep_folder_and_extension() {
        let now = Stamp::new(1_700_000_000, 0);
        assert_eq!(
            conflict_rel("a/report.final.pdf", "remote", "0123456789abcdef", now),
            "a/report.final.sync-conflict-20231114-221320-remote-01234567.pdf"
        );
        assert_eq!(
            conflict_rel(".bashrc", "local", "abcdef01", now),
            ".bashrc.sync-conflict-20231114-221320-local-abcdef01"
        );
        let long = "x".repeat(300);
        assert!(conflict_rel(&long, "local", "abcdef01", now).len() <= 255);
    }

    #[test]
    fn conflict_copies_map_back_to_their_item() {
        let now = Stamp::new(1_700_000_000, 0);
        let copy = conflict_rel("a/report.pdf", "local", "abcdef01", now);
        assert_eq!(
            conflict_origin(&copy),
            Some(("a/report.pdf".into(), "20231114-221320".into()))
        );
        assert_eq!(conflict_origin("a/report.pdf"), None);
    }

    #[test]
    fn no_replace_rename_refuses_to_overwrite_and_exchange_swaps() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "a").unwrap();
        fs::write(temp.path().join("b"), "b").unwrap();
        let root = Root::open(temp.path(), None).unwrap();
        assert!(root.rename_noreplace("a", "b").is_err());
        assert_eq!(fs::read_to_string(temp.path().join("b")).unwrap(), "b");
        root.exchange("a", "b").unwrap();
        assert_eq!(fs::read_to_string(temp.path().join("a")).unwrap(), "b");
        assert_eq!(fs::read_to_string(temp.path().join("b")).unwrap(), "a");
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(random_id(), random_id());
        assert_eq!(random_id().len(), 16);
    }
}
