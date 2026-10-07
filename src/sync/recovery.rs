//! Preserved versions: the pair's internal folder, recovery copies of every
//! version Celestial replaces or deletes, conflict-copy names and retention.
//!
//! Local recovery lives in an internal folder that Celestial creates inside
//! the pair root (same filesystem, so preservation is a rename). Remote
//! recovery lives in `.celestial-recovery/<pair>` at the root of the remote,
//! outside the synced folder unless the whole remote is synced, and is
//! visible to other clients of the account. Provider trash is never relied
//! on.

use super::observe::{LocalMeta, RemoteMeta, Stamp};
use super::store::{self, RecoveryItem};
use crate::entities::{RemotesModel, SyncDirsModel};
use crate::rclone;
use nix::fcntl::{RenameFlags, renameat2};
use sea_orm::DatabaseConnection;
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
pub const KEEP_SECS: i64 = 30 * 24 * 3600;
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
    let recorded = store::internal_dir(db, dir.id).map_err(|err| err.to_string())?;
    if let Some((name, identity)) = &recorded {
        let path = root.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.is_dir()
                && (metadata.dev(), metadata.ino()) == *identity
                && fs::read_to_string(path.join(MARKER)).ok().as_deref() == Some(&pair_uuid)
            {
                let internal = Internal {
                    name: name.clone(),
                    path,
                    identity: *identity,
                    pair_uuid,
                };
                create_subfolders(&internal)?;
                return Ok(internal);
            }
        }
    }
    let root_dev = fs::metadata(root).map_err(|err| err.to_string())?.dev();
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

/// Rename without replacing anything at the destination.
pub fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    renameat2(None, from, None, to, RenameFlags::RENAME_NOREPLACE).map_err(std::io::Error::from)
}

/// Swap two existing paths atomically.
pub fn rename_exchange(first: &Path, second: &Path) -> std::io::Result<()> {
    renameat2(None, first, None, second, RenameFlags::RENAME_EXCHANGE).map_err(std::io::Error::from)
}

/// Where everything for one pair lives.
pub struct Store<'a> {
    pub db: &'a DatabaseConnection,
    pub remote: &'a RemotesModel,
    pub dir: &'a SyncDirsModel,
    pub internal: &'a Internal,
}

impl Store<'_> {
    fn version_folder(&self, op: &str) -> String {
        format!("{}-{}", Stamp::now().secs, op)
    }

    /// The remote folder that keeps this pair's preserved versions.
    pub fn remote_root(&self) -> String {
        format!("{REMOTE_RECOVERY_ROOT}/{}", self.internal.pair_uuid)
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
        let source = Path::new(&self.dir.local_path).join(rel);
        self.preserve_path(&source, rel, expected, reason, op)
    }

    /// Preserve the file at `source` (inside the pair) as a version of `rel`.
    pub fn preserve_path(
        &self,
        source: &Path,
        rel: &str,
        expected: &LocalMeta,
        reason: &str,
        op: &str,
    ) -> Result<RecoveryItem, String> {
        let target = self
            .internal
            .recovery()
            .join(self.version_folder(op))
            .join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        same_filesystem(source, &self.internal.path)?;
        rename_noreplace(source, &target)
            .map_err(|err| tr::tr!("Could not move the previous version aside [{}].", err))?;
        let moved = fs::symlink_metadata(&target).map_err(|err| err.to_string())?;
        if !LocalMeta::from_metadata(&moved).same_after_rename(expected) {
            // Something else was at the path by then: leave it where it was.
            let _ = rename_noreplace(&target, source);
            return Err(tr::tr!("The local item changed after it was checked."));
        }
        self.record(
            rel,
            "local",
            target.display().to_string(),
            reason,
            op,
            Some(i64::try_from(expected.size).unwrap_or(i64::MAX)),
            Some(expected.mtime.secs),
        )
        .inspect_err(|_| {
            // Unrecorded preservation would be invisible: put it back.
            let _ = rename_noreplace(&target, source);
        })
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
        rclone::sync::move_on_remote(&self.remote.name, &source, &target)
            .map_err(|err| tr::tr!("Could not move the previous version aside [{}].", err))?;
        let moved = rclone::sync::stat_with(&self.remote.name, &target, hash_opts)
            .map_err(|err| err.error)?
            .map(|item| RemoteMeta::from_item(&item, hash_type))
            .ok_or_else(|| tr::tr!("The preserved remote version could not be found."))?;
        let item = self
            .record(
                rel,
                "remote",
                target.clone(),
                reason,
                op,
                Some(moved.size),
                Some(moved.mtime.secs),
            )
            .inspect_err(|_| {
                if matches!(rclone::sync::stat(&self.remote.name, &source), Ok(None)) {
                    let _ = rclone::sync::move_on_remote(&self.remote.name, &target, &source);
                }
            })?;
        if &moved != expected {
            tracing::warn!(rel, "remote item changed before it was preserved");
        }
        Ok((item, moved))
    }

    /// Delete preserved versions beyond the retention policy: the newest
    /// versions of each item are kept for a limited time, but never those of
    /// unresolved conflicts or incomplete operations, nor the only copy of
    /// an item that no longer exists anywhere.
    pub fn prune(&self, live: &HashSet<String>) -> Result<usize, String> {
        let items = store::list_recovery(self.db, &self.internal.pair_uuid)
            .map_err(|err| err.to_string())?;
        let protected = store::protected_operations(self.db, &self.internal.pair_uuid)
            .map_err(|err| err.to_string())?;
        let mut by_item: BTreeMap<&str, Vec<&RecoveryItem>> = BTreeMap::new();
        for item in &items {
            by_item.entry(item.rel.as_str()).or_default().push(item);
        }
        let now = store::now();
        let mut pruned = 0;
        for (rel, versions) in by_item {
            // Already sorted newest first.
            for (index, item) in versions.iter().enumerate() {
                let fresh = now - item.created_at < KEEP_SECS;
                let only_copy = index == 0 && !live.contains(rel);
                if (index < KEEP_VERSIONS && fresh) || only_copy || protected.contains(&item.op_id)
                {
                    continue;
                }
                let removed = if item.side == "local" {
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
                };
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
}

fn same_filesystem(path: &Path, internal: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("Invalid path")?;
    let parent_dev = fs::symlink_metadata(parent)
        .map_err(|err| err.to_string())?
        .dev();
    let internal_dev = fs::symlink_metadata(internal)
        .map_err(|err| err.to_string())?
        .dev();
    if parent_dev == internal_dev {
        Ok(())
    } else {
        Err(tr::tr!(
            "This item is on another filesystem than the sync folder; it is paused so it can be preserved before it is replaced."
        ))
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
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        assert!(rename_noreplace(&a, &b).is_err());
        assert_eq!(fs::read_to_string(&b).unwrap(), "b");
        rename_exchange(&a, &b).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "b");
        assert_eq!(fs::read_to_string(&b).unwrap(), "a");
    }

    #[test]
    fn random_ids_differ() {
        assert_ne!(random_id(), random_id());
        assert_eq!(random_id().len(), 16);
    }
}
