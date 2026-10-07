//! Observations of both sides of a pair.
//!
//! An observation is `Present`, `Missing` or `Unknown`. Only a complete,
//! successful read can say an item is missing; a failed or partial read is
//! `Unknown` and never authorizes a transfer or a deletion.

use crate::rclone::{self, RcloneRemoteItem};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use time::OffsetDateTime;
use unicode_normalization::UnicodeNormalization;

/// A signed timestamp with nanoseconds. Times before the Unix epoch and after
/// 2038 are both representable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Stamp {
    pub secs: i64,
    pub nanos: u32,
}

/// Filesystem timestamp granularity assumed by the racy-clean rule, in
/// nanoseconds. Tests shorten it so they need not sleep.
static RACY_WINDOW_NANOS: AtomicI64 = AtomicI64::new(2_000_000_000);

#[cfg(test)]
pub fn set_racy_window(nanos: i64) {
    RACY_WINDOW_NANOS.store(nanos, Ordering::Relaxed);
}

impl Stamp {
    pub fn new(secs: i64, nanos: i64) -> Self {
        Self {
            secs,
            nanos: u32::try_from(nanos.clamp(0, 999_999_999)).unwrap_or(0),
        }
    }

    pub fn from_datetime(time: OffsetDateTime) -> Self {
        Self {
            secs: time.unix_timestamp(),
            nanos: time.nanosecond(),
        }
    }

    pub fn now() -> Self {
        Self::from_datetime(OffsetDateTime::now_utc())
    }

    pub fn total_nanos(self) -> i128 {
        i128::from(self.secs) * 1_000_000_000 + i128::from(self.nanos)
    }

    /// Whether two times are indistinguishable at `precision_ns`.
    pub fn same_within(self, other: Self, precision_ns: i64) -> bool {
        (self.total_nanos() - other.total_nanos()).abs() < i128::from(precision_ns.max(1))
    }

    pub fn to_datetime(self) -> Option<OffsetDateTime> {
        OffsetDateTime::from_unix_timestamp_nanos(self.total_nanos()).ok()
    }
}

impl std::fmt::Display for Stamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_datetime().and_then(|time| {
            time.format(&time::format_description::well_known::Rfc3339)
                .ok()
        }) {
            Some(text) => f.write_str(&text),
            None => write!(f, "{}.{:09}", self.secs, self.nanos),
        }
    }
}

/// One observation of an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Obs<T> {
    Present(T),
    Missing,
    Unknown(String),
}

impl<T> Obs<T> {
    pub fn present(&self) -> Option<&T> {
        match self {
            Self::Present(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

/// Local metadata used to detect changes without reading contents. Tools that
/// preserve the modification time still change the inode change time, which
/// user space cannot set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalMeta {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Stamp,
    pub ctime: Stamp,
    pub inode: u64,
    pub device: u64,
}

impl LocalMeta {
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            is_dir: metadata.is_dir(),
            size: if metadata.is_dir() {
                0
            } else {
                metadata.size()
            },
            mtime: Stamp::new(metadata.mtime(), metadata.mtime_nsec()),
            ctime: Stamp::new(metadata.ctime(), metadata.ctime_nsec()),
            inode: metadata.ino(),
            device: metadata.dev(),
        }
    }

    /// Whether the change-detection tuple is identical.
    pub fn same_tuple(&self, other: &Self) -> bool {
        if self.is_dir || other.is_dir {
            return self.is_dir == other.is_dir;
        }
        self == other
    }

    /// Whether this is the same file, unchanged, after Celestial itself
    /// renamed it (a rename updates the inode change time, so ctime cannot
    /// be compared here).
    pub fn same_after_rename(&self, other: &Self) -> bool {
        self.is_dir == other.is_dir
            && self.inode == other.inode
            && self.device == other.device
            && (self.is_dir || (self.size == other.size && self.mtime == other.mtime))
    }

    /// A file changed within the timestamp granularity of the observation
    /// could change again without changing its tuple, so its fingerprint is
    /// not trusted as a baseline until a later observation. Every content
    /// change sets the inode change time to the current time; the
    /// modification time is not used because tools set it freely, including
    /// to the future.
    pub fn is_racy(&self, observed_at: Stamp) -> bool {
        let limit =
            observed_at.total_nanos() - i128::from(RACY_WINDOW_NANOS.load(Ordering::Relaxed));
        !self.is_dir && self.ctime.total_nanos() >= limit
    }
}

/// Remote metadata from a listing. The hash is in the pair's preferred type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMeta {
    pub is_dir: bool,
    pub size: i64,
    pub mtime: Stamp,
    pub hash: Option<String>,
    pub id: Option<String>,
}

impl RemoteMeta {
    pub fn from_item(item: &RcloneRemoteItem, hash_type: Option<&str>) -> Self {
        Self {
            is_dir: item.is_dir,
            size: if item.is_dir { 0 } else { item.size },
            mtime: Stamp::from_datetime(item.mod_time),
            hash: hash_type
                .and_then(|kind| item.hashes.get(kind))
                .filter(|hash| !hash.is_empty())
                .cloned(),
            id: item.id.clone(),
        }
    }
}

/// A content hash and the algorithm that produced it. Hashes are only
/// compared with hashes of the same algorithm; a missing hash never stands
/// for any particular contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub algo: String,
    pub value: String,
}

/// The local tree of a pair, by path relative to the pair root.
#[derive(Debug, Default)]
pub struct LocalTree {
    pub items: BTreeMap<String, LocalMeta>,
    /// Items that cannot be synchronized, with the reason.
    pub blocked: BTreeMap<String, String>,
    /// Folders that could not be read; everything below them is unknown.
    pub unreadable: BTreeMap<String, String>,
}

impl LocalTree {
    pub fn observe(&self, rel: &str) -> Obs<LocalMeta> {
        if let Some(meta) = self.items.get(rel) {
            return Obs::Present(*meta);
        }
        if let Some(reason) = self.blocked.get(rel) {
            return Obs::Unknown(reason.clone());
        }
        if let Some(reason) = under(&self.unreadable, rel) {
            return Obs::Unknown(reason);
        }
        Obs::Missing
    }
}

/// The remote tree of a pair, by path relative to the pair root.
#[derive(Debug, Default)]
pub struct RemoteTree {
    pub items: BTreeMap<String, RemoteMeta>,
    pub blocked: BTreeMap<String, String>,
}

impl RemoteTree {
    pub fn observe(&self, rel: &str) -> Obs<RemoteMeta> {
        if let Some(reason) = self.blocked.get(rel) {
            return Obs::Unknown(reason.clone());
        }
        match self.items.get(rel) {
            Some(meta) => Obs::Present(meta.clone()),
            None => Obs::Missing,
        }
    }
}

/// The reason recorded for `rel` or one of its ancestors.
fn under(map: &BTreeMap<String, String>, rel: &str) -> Option<String> {
    let mut path = rel;
    loop {
        if let Some(reason) = map.get(path) {
            return Some(reason.clone());
        }
        path = path.rsplit_once('/')?.0;
    }
}

pub const NON_UTF8: &str = "The name is not valid UTF-8, so it cannot be represented on the remote without changing it. Rename it to synchronize it.";
pub const NOT_REGULAR: &str = "Only regular files and folders are synchronized; symbolic links and special files are skipped.";
pub const LEFTOVER_PARTIAL: &str = "This looks like an unfinished download left by an earlier version. It is not uploaded; delete it if the complete file is present.";

/// rclone's temporary download name: `<name>.<8 characters>.partial`.
pub fn is_rclone_partial(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".partial") else {
        return false;
    };
    let Some((base, tag)) = stem.rsplit_once('.') else {
        return false;
    };
    !base.is_empty() && tag.len() == 8 && tag.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Walk the local tree without following symbolic links or opening files.
/// `skip` receives the relative path and the item's metadata and excludes the
/// item (and everything under it) when it returns `true`. A failure to read
/// the root is an error; a failure below it makes only that subtree unknown.
pub fn observe_local(
    root: &Path,
    skip: &dyn Fn(&str, &fs::Metadata) -> bool,
) -> std::io::Result<LocalTree> {
    let mut tree = LocalTree::default();
    let mut pending = vec![(root.to_path_buf(), String::new())];
    let mut first = true;
    while let Some((dir, prefix)) = pending.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if first => return Err(err),
            Err(err) => {
                tree.unreadable.insert(prefix, err.to_string());
                continue;
            }
        };
        first = false;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    // The folder listing is incomplete: nothing under it is missing.
                    tree.unreadable.insert(prefix.clone(), err.to_string());
                    break;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                let lossy = String::from_utf8_lossy(name.as_bytes()).into_owned();
                tree.blocked.insert(join(&prefix, &lossy), NON_UTF8.into());
                continue;
            };
            let rel = join(&prefix, name);
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    tree.blocked.insert(rel, err.to_string());
                    continue;
                }
            };
            if skip(&rel, &metadata) {
                continue;
            }
            if !metadata.is_dir() && !metadata.is_file() {
                tree.blocked.insert(rel, NOT_REGULAR.into());
                continue;
            }
            if metadata.is_file() && is_rclone_partial(name) {
                tree.blocked.insert(rel, LEFTOVER_PARTIAL.into());
                continue;
            }
            if metadata.is_dir() {
                pending.push((entry.path(), rel.clone()));
            }
            tree.items.insert(rel, LocalMeta::from_metadata(&metadata));
        }
    }
    Ok(tree)
}

pub fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Build the remote tree from a recursive listing of `remote_root`. Paths
/// outside the root or containing traversal are blocked, as are several
/// objects sharing one path.
pub fn remote_tree(
    remote_root: &str,
    items: &[RcloneRemoteItem],
    hash_type: Option<&str>,
    skip: &dyn Fn(&str) -> bool,
) -> RemoteTree {
    let mut tree = RemoteTree::default();
    for item in items {
        let rel = if remote_root.is_empty() {
            Some(item.path.as_str())
        } else {
            item.path
                .strip_prefix(remote_root)
                .and_then(|rest| rest.strip_prefix('/'))
        };
        let Some(rel) = rel.filter(|rel| valid_relative(rel)) else {
            tree.blocked.insert(
                item.path.clone(),
                "The remote reported a path outside the synced folder".into(),
            );
            continue;
        };
        if skip(rel) {
            continue;
        }
        if tree.blocked.contains_key(rel) {
            continue;
        }
        if tree.items.contains_key(rel) {
            tree.items.remove(rel);
            tree.blocked.insert(
                rel.to_owned(),
                "Several remote items share this name. Rename all but one of them on the remote."
                    .into(),
            );
            continue;
        }
        tree.items
            .insert(rel.to_owned(), RemoteMeta::from_item(item, hash_type));
    }
    // Items below an excluded or blocked folder are not synchronized either.
    let blocked: Vec<String> = tree.blocked.keys().cloned().collect();
    tree.items.retain(|rel, _| {
        !blocked
            .iter()
            .any(|parent| rel.starts_with(&format!("{parent}/")))
    });
    tree
}

pub fn valid_relative(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\0')
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Find names that would map onto the same item: Unicode normalization
/// variants everywhere, and names differing only by case when the remote is
/// case-insensitive. Every group of two or more distinct names is returned.
pub fn collision_groups<'a>(
    paths: impl IntoIterator<Item = &'a str>,
    case_insensitive: bool,
) -> Vec<Vec<String>> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in paths {
        let mut key: String = path.nfc().collect();
        if case_insensitive {
            key = key.to_lowercase();
        }
        let group = groups.entry(key).or_default();
        if !group.iter().any(|known| known == path) {
            group.push(path.to_owned());
        }
    }
    groups
        .into_values()
        .filter(|group| group.len() > 1)
        .collect()
}

pub fn collision_reason(group: &[String]) -> String {
    format!(
        "'{}' would become the same item on the remote (letter case or Unicode normalization). Rename all but one of them.",
        group.join("', '")
    )
}

/// Bytes of local files read for hashing, for measurement.
pub static HASHED_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Why hashing a local file did not produce a fingerprint.
#[derive(Debug, PartialEq, Eq)]
pub enum HashError {
    /// The file changed while it was read; retry later.
    Changing,
    Failed(String),
}

/// Hash a local file in the provider's algorithm through rclone, refusing a
/// result when the file changed while it was being read.
pub fn hash_local(path: &Path, algo: &str) -> Result<Option<Fingerprint>, HashError> {
    let before = fs::symlink_metadata(path).map_err(|err| HashError::Failed(err.to_string()))?;
    let text = path
        .to_str()
        .ok_or_else(|| HashError::Failed(NON_UTF8.into()))?;
    HASHED_BYTES.fetch_add(before.len(), Ordering::Relaxed);
    let value = rclone::sync::local_hash(text, algo).map_err(|err| HashError::Failed(err.error))?;
    let after = fs::symlink_metadata(path).map_err(|err| HashError::Failed(err.to_string()))?;
    if !LocalMeta::from_metadata(&before).same_tuple(&LocalMeta::from_metadata(&after)) {
        return Err(HashError::Changing);
    }
    Ok(value.map(|value| Fingerprint {
        algo: algo.to_owned(),
        value,
    }))
}

/// A short token describing what a person saw of both sides, so an action can
/// be refused when either side changed afterwards.
pub fn content_token(local: &Obs<LocalMeta>, remote: &Obs<RemoteMeta>) -> String {
    let local = match local {
        Obs::Present(meta) => format!(
            "L{}:{}:{}:{}:{}",
            u8::from(meta.is_dir),
            meta.size,
            meta.mtime.total_nanos(),
            meta.ctime.total_nanos(),
            meta.inode
        ),
        Obs::Missing => "L-".into(),
        Obs::Unknown(_) => "L?".into(),
    };
    let remote = match remote {
        Obs::Present(meta) => format!(
            "R{}:{}:{}:{}",
            u8::from(meta.is_dir),
            meta.size,
            meta.mtime.total_nanos(),
            meta.hash.as_deref().unwrap_or("")
        ),
        Obs::Missing => "R-".into(),
        Obs::Unknown(_) => "R?".into(),
    };
    format!("{local}|{remote}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn stamps_before_the_epoch_and_after_2038_order_correctly() {
        let before = Stamp::new(-1, 500_000_000);
        let epoch = Stamp::new(0, 0);
        let later = Stamp::new(4_000_000_000, 1);
        assert!(before < epoch && epoch < later);
        assert!(Stamp::new(10, 1).same_within(Stamp::new(10, 999_999_999), 1_000_000_000));
        assert!(!Stamp::new(10, 1).same_within(Stamp::new(10, 2), 1));
    }

    #[test]
    fn preserved_mtime_still_changes_the_tuple() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("f");
        fs::write(&file, "aaaa").unwrap();
        let before = LocalMeta::from_metadata(&fs::metadata(&file).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&file, "bbbb").unwrap();
        fs::File::open(&file)
            .unwrap()
            .set_modified(
                std::time::UNIX_EPOCH
                    + std::time::Duration::new(before.mtime.secs as u64, before.mtime.nanos),
            )
            .unwrap();
        let after = LocalMeta::from_metadata(&fs::metadata(&file).unwrap());
        assert_eq!(before.mtime, after.mtime);
        assert_eq!(before.size, after.size);
        assert!(!before.same_tuple(&after));
    }

    #[test]
    fn non_utf8_names_are_blocked_not_mapped_to_empty_paths() {
        let temp = tempfile::tempdir().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"bad\xffname");
        fs::write(temp.path().join(name), "x").unwrap();
        fs::write(temp.path().join("good"), "x").unwrap();
        let tree = observe_local(temp.path(), &|_, _| false).unwrap();
        assert!(tree.items.contains_key("good"));
        assert!(!tree.items.contains_key(""));
        assert_eq!(tree.blocked.len(), 1);
        assert!(tree.blocked.values().all(|reason| reason == NON_UTF8));
    }

    #[test]
    fn leftover_partial_downloads_are_reported_not_uploaded() {
        assert!(is_rclone_partial("report.pdf.a1B2c3D4.partial"));
        assert!(!is_rclone_partial("report.partial"));
        assert!(!is_rclone_partial(".a1B2c3D4.partial"));
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("doc.txt.abcdefgh.partial"), "x").unwrap();
        let tree = observe_local(temp.path(), &|_, _| false).unwrap();
        assert!(tree.items.is_empty());
        assert_eq!(
            tree.observe("doc.txt.abcdefgh.partial"),
            Obs::Unknown(LEFTOVER_PARTIAL.into())
        );
    }

    #[test]
    fn unreadable_folders_are_unknown_not_missing() {
        let tree = LocalTree {
            unreadable: [("a".to_owned(), "denied".to_owned())].into(),
            ..Default::default()
        };
        assert_eq!(tree.observe("a/b/c"), Obs::Unknown("denied".into()));
        assert_eq!(tree.observe("b"), Obs::Missing);
    }

    #[test]
    fn case_and_normalization_collisions_are_found() {
        let found = collision_groups(["Readme.md", "README.md", "other", "README.md"], true);
        assert_eq!(
            found,
            vec![vec!["Readme.md".to_owned(), "README.md".to_owned()]]
        );
        assert!(collision_groups(["Readme.md", "README.md"], false).is_empty());
        let composed = "caf\u{e9}";
        let decomposed = "cafe\u{301}";
        assert_eq!(collision_groups([composed, decomposed], false)[0].len(), 2);
    }

    #[test]
    fn duplicate_and_escaping_remote_paths_are_blocked() {
        let item = |path: &str| RcloneRemoteItem {
            is_dir: false,
            path: path.into(),
            name: path.rsplit('/').next().unwrap().into(),
            mod_time: OffsetDateTime::UNIX_EPOCH,
            size: 1,
            hashes: Default::default(),
            id: None,
        };
        let tree = remote_tree(
            "root",
            &[
                item("root/a"),
                item("root/a"),
                item("root/../x"),
                item("root/b"),
            ],
            None,
            &|_| false,
        );
        assert!(tree.items.contains_key("b"));
        assert!(matches!(tree.observe("a"), Obs::Unknown(_)));
        assert_eq!(tree.blocked.len(), 2);
    }
}
