//! Files another program is still writing.
//!
//! A file copied while a recorder, a download or an export still appends to
//! it reaches the remote truncated, and every later write uploads it again
//! from the start, each time preserving the previous partial copy as a
//! replaced version. A pass therefore leaves a local file alone while it was
//! modified within [`QUIET_SECS`], or while a process still holds it open for
//! writing and modified it within [`OPEN_WRITE_SECS`]. A file held open but
//! untouched for longer (a log or a database kept open all day) is
//! synchronized as before: the upload still checks that it did not change
//! while it was read.

use super::observe::{LocalMeta, Stamp};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A file modified more recently than this may still be in the middle of
/// being written.
pub const QUIET_SECS: i64 = 5;
/// A file open for writing counts as being written while it was modified
/// within this long. Recorders and downloads write far more often.
pub const OPEN_WRITE_SECS: i64 = 120;
/// How soon a file that is open for writing is looked at again.
pub const RECHECK_SECS: i64 = 10;

#[cfg(test)]
thread_local! {
    static TEST_QUIET_SECS: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Tests write files right before a pass; only the tests about writers ask
/// for the check, on their own thread, with the quiet age they need.
#[cfg(test)]
pub fn enable_for_test(quiet_secs: i64) {
    TEST_QUIET_SECS.with(|quiet| quiet.set(Some(quiet_secs)));
}

/// The quiet age, or `None` when files are never held.
fn quiet_secs() -> Option<i64> {
    #[cfg(test)]
    return TEST_QUIET_SECS.with(std::cell::Cell::get);
    #[cfg(not(test))]
    Some(QUIET_SECS)
}

/// Why and until when a file waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub until: i64,
    pub reason: String,
}

/// The files below one pair root, read once per pass and only when needed.
pub struct Writers {
    root: PathBuf,
    open: OnceLock<HashSet<PathBuf>>,
}

impl Writers {
    pub fn new(root: &Path) -> Self {
        Self {
            root: fs::canonicalize(root).unwrap_or_else(|_| root.to_owned()),
            open: OnceLock::new(),
        }
    }

    /// Whether the file at `rel`, observed as `meta`, must wait before it is
    /// synchronized, and until when.
    pub fn wait(&self, rel: &str, meta: &LocalMeta, now: Stamp) -> Option<Wait> {
        let quiet = quiet_secs()?;
        if meta.is_dir {
            return None;
        }
        let changed = meta.mtime.max(meta.ctime);
        let age = now.total_nanos() - changed.total_nanos();
        // A time far in the future is a clock problem, not a writer.
        if age < -i128::from(QUIET_SECS) * 1_000_000_000 {
            return None;
        }
        if age < i128::from(quiet) * 1_000_000_000 {
            return Some(Wait {
                // Whole seconds: one more covers the fraction.
                until: (changed.secs + quiet + 1).max(now.secs + 1),
                reason: tr::tr!("The file was just changed; waiting until it stops changing."),
            });
        }
        if age < i128::from(OPEN_WRITE_SECS) * 1_000_000_000
            && self
                .open
                .get_or_init(|| open_for_writing(&self.root))
                .contains(&self.root.join(rel))
        {
            return Some(Wait {
                until: now.secs + RECHECK_SECS,
                reason: tr::tr!(
                    "Another program is still writing this file; it will be synchronized once it is closed."
                ),
            });
        }
        None
    }
}

/// The files below `root` that some visible process has open for writing,
/// from `/proc/<pid>/fd` and the access mode in `/proc/<pid>/fdinfo`.
/// Processes of other users are not visible; the age rule still covers
/// them.
fn open_for_writing(root: &Path) -> HashSet<PathBuf> {
    let mut open = HashSet::new();
    let Ok(procs) = fs::read_dir("/proc") else {
        return open;
    };
    for proc in procs.flatten() {
        let name = proc.file_name();
        if !name
            .to_str()
            .is_some_and(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let Ok(fds) = fs::read_dir(proc.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            if !target.starts_with(root) || open.contains(&target) {
                continue;
            }
            let info = proc.path().join("fdinfo").join(fd.file_name());
            if fs::read_to_string(info).is_ok_and(|info| writable(&info)) {
                open.insert(target);
            }
        }
    }
    open
}

/// Whether an fdinfo's `flags:` (octal open flags) allow writing.
fn writable(fdinfo: &str) -> bool {
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
        .is_some_and(|flags| flags & 0o3 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn meta(path: &Path) -> LocalMeta {
        LocalMeta::from_metadata(&fs::metadata(path).unwrap())
    }

    fn later(meta: &LocalMeta, secs: i64) -> Stamp {
        Stamp::new(meta.mtime.max(meta.ctime).secs + secs, 0)
    }

    #[test]
    fn fdinfo_access_mode() {
        assert!(!writable("pos:\t0\nflags:\t0100000\nmnt_id:\t1\n"));
        assert!(writable("pos:\t0\nflags:\t0100001\n"));
        assert!(writable("pos:\t0\nflags:\t02100002\n"));
        assert!(!writable("pos:\t0\n"));
    }

    #[test]
    fn a_just_modified_file_waits_for_the_quiet_age() {
        enable_for_test(QUIET_SECS);
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let meta = meta(&dir.path().join("a.txt"));
        let writers = Writers::new(dir.path());
        let now = later(&meta, 1);
        let wait = writers.wait("a.txt", &meta, now).unwrap();
        assert_eq!(wait.until, now.secs + QUIET_SECS);
        assert_eq!(
            writers.wait("a.txt", &meta, later(&meta, QUIET_SECS + 1)),
            None
        );
        // A clock far behind the file's time does not hold it forever.
        assert_eq!(writers.wait("a.txt", &meta, later(&meta, -60)), None);
    }

    #[test]
    fn a_file_open_for_writing_waits_until_it_is_closed() {
        enable_for_test(QUIET_SECS);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mkv");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"header").unwrap();
        let meta = meta(&path);
        let now = later(&meta, QUIET_SECS + 1);
        let wait = Writers::new(dir.path())
            .wait("recording.mkv", &meta, now)
            .unwrap();
        assert_eq!(wait.until, now.secs + RECHECK_SECS);
        // Untouched for long, an open file is synchronized anyway.
        let stale = later(&meta, OPEN_WRITE_SECS + 1);
        assert_eq!(
            Writers::new(dir.path()).wait("recording.mkv", &meta, stale),
            None
        );
        drop(file);
        assert_eq!(
            Writers::new(dir.path()).wait("recording.mkv", &meta, now),
            None
        );
        // Open only for reading is not writing.
        let _reader = fs::File::open(&path).unwrap();
        assert_eq!(
            Writers::new(dir.path()).wait("recording.mkv", &meta, now),
            None
        );
    }
}
