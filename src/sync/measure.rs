//! Estimate a folder before it is synchronized: how many files and bytes the
//! local folder holds (after exclusions), what the remote folder already
//! holds and how much space the remote has left.
//!
//! Runs on its own thread so a large folder or a slow remote never delays
//! synchronization. A newer request makes older ones stop.

use super::exclusions::Rules;
use super::localfs::Root;
use super::progress::{format_bytes, format_count};
use super::{Notice, Shared};
use crate::rclone::{self, FolderSize};
use serde_json::json;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The latest request; older ones stop at their next check.
static LATEST: AtomicU64 = AtomicU64::new(0);

/// Partial counts are reported at most this often.
const REPORT_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LocalSize {
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct Estimate {
    remote: String,
    local: String,
    remote_path: String,
    done: bool,
    local_size: LocalSize,
    local_error: Option<String>,
    remote_size: Option<FolderSize>,
    remote_error: Option<String>,
    free: Option<i64>,
}

impl Estimate {
    fn json(&self) -> String {
        let mut lines = Vec::new();
        match &self.local_error {
            Some(err) => lines.push(tr::tr!("The local folder could not be read [{}].", err)),
            None => {
                let counted = tr::tr!(
                    "This folder: {} files, {}",
                    format_count(self.local_size.files),
                    format_bytes(self.local_size.bytes)
                );
                lines.push(if self.done {
                    counted
                } else {
                    format!("{counted} {}", tr::tr!("(counting...)"))
                });
            }
        }
        match (&self.remote_size, &self.remote_error) {
            (Some(size), _) => lines.push(tr::tr!(
                "Remote folder: {} files, {}",
                format_count(size.count),
                format_bytes(size.bytes)
            )),
            (None, Some(err)) => lines.push(tr::tr!(
                "The remote folder could not be measured [{}].",
                err
            )),
            (None, None) if !self.done => lines.push(tr::tr!("Measuring the remote folder...")),
            (None, None) => {}
        }
        if let Some(free) = self.free {
            lines.push(tr::tr!(
                "{} free on the remote.",
                format_bytes(u64::try_from(free).unwrap_or(0))
            ));
        }
        json!({
            "remote": self.remote,
            "local": self.local,
            "remotePath": self.remote_path,
            "done": self.done,
            "localFiles": self.local_size.files,
            "localBytes": self.local_size.bytes,
            "remoteFiles": self.remote_size.map(|size| size.count),
            "remoteBytes": self.remote_size.map(|size| size.bytes),
            "free": self.free,
            "text": lines.join("\n"),
            "warning": self.warning(),
        })
        .to_string()
    }

    /// Files already on the remote are not uploaded again, so only what the
    /// local folder holds beyond them has to fit.
    fn warning(&self) -> Option<String> {
        let free = u64::try_from(self.free?).unwrap_or(0);
        let present = self.remote_size.map_or(0, |size| size.bytes);
        let needed = self.local_size.bytes.saturating_sub(present);
        (self.done && self.local_error.is_none() && needed > free).then(|| {
            tr::tr!(
                "Up to {} may need to be uploaded, but only {} is free on the remote. Celestial will ask before uploading more than fits.",
                format_bytes(needed),
                format_bytes(free)
            )
        })
    }
}

/// Stop any estimate in progress.
pub fn cancel() {
    LATEST.fetch_add(1, Ordering::Relaxed);
}

/// Estimate a folder pair, reporting partial results as they come.
pub fn request(shared: &Shared, remote: String, local: String, remote_path: String) {
    let id = LATEST.fetch_add(1, Ordering::Relaxed) + 1;
    let shared = shared.clone();
    let spawned = thread::Builder::new()
        .name("celestial-measure".into())
        .spawn(move || {
            let stale = || LATEST.load(Ordering::Relaxed) != id;
            let send = |estimate: &Estimate| {
                if !stale() {
                    let _ = shared.notices.send(Notice::Estimate(estimate.json()));
                }
            };
            let mut estimate = Estimate {
                remote,
                local,
                remote_path,
                ..Estimate::default()
            };
            send(&estimate);
            thread::scope(|scope| {
                let remote_job = scope.spawn(|| {
                    (
                        rclone::sync::folder_size(&estimate.remote, &estimate.remote_path),
                        rclone::sync::free_space(&estimate.remote),
                    )
                });
                let mut partial = estimate.clone();
                let counted = local_size(Path::new(&estimate.local), &stale, &mut |size| {
                    partial.local_size = size;
                    send(&partial);
                });
                let (size, free) = remote_job
                    .join()
                    .unwrap_or_else(|_| (Err(rclone::RcloneError::new("failed")), Ok(None)));
                match counted {
                    Ok(Some(size)) => estimate.local_size = size,
                    Ok(None) => return,
                    Err(err) => estimate.local_error = Some(err),
                }
                match size {
                    Ok(size) => estimate.remote_size = Some(size),
                    Err(err) => estimate.remote_error = Some(err.error),
                }
                estimate.free = free.unwrap_or(None);
                estimate.done = true;
                send(&estimate);
            });
        });
    if let Err(err) = spawned {
        tracing::warn!("could not start measuring a folder: {err}");
    }
}

/// Count the regular files a pass would synchronize under `root`, with the
/// exclusions it would use. `None` when `stale` stopped the count.
pub fn local_size(
    root: &Path,
    stale: &dyn Fn() -> bool,
    report: &mut dyn FnMut(LocalSize),
) -> Result<Option<LocalSize>, String> {
    if !root.is_absolute() {
        return Err(tr::tr!(
            "The specified local directory needs to be an absolute path"
        ));
    }
    let rules = Rules::preview(root)?;
    let handle = Root::open(root, None).map_err(|err| err.to_string())?;
    let mut size = LocalSize::default();
    let mut pending = vec![String::new()];
    let mut reported = Instant::now();
    while let Some(rel) = pending.pop() {
        if stale() {
            return Ok(None);
        }
        let entries = match fs::read_dir(root.join(&rel)) {
            Ok(entries) => entries,
            Err(err) if rel.is_empty() => return Err(err.to_string()),
            // An unreadable subfolder is reported by the pass itself.
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if rules.matches_in(&child, &handle) {
                continue;
            }
            // Symbolic links are not followed, as in a pass.
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(child);
            } else if metadata.is_file() {
                size.files += 1;
                size.bytes += metadata.len();
            }
        }
        if reported.elapsed() >= REPORT_INTERVAL {
            reported = Instant::now();
            report(size);
        }
    }
    Ok(Some(size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::FILE_IGNORE_NAME;

    #[test]
    fn local_size_counts_files_after_exclusions_without_writing_rules() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("skip")).unwrap();
        fs::write(root.join("one.txt"), "12345").unwrap();
        fs::write(root.join("a/b/two.txt"), "123").unwrap();
        fs::write(root.join("skip/big.bin"), vec![0; 1000]).unwrap();
        fs::write(root.join("a/note.tmp"), "xx").unwrap();
        std::os::unix::fs::symlink(root.join("skip"), root.join("link")).unwrap();
        // No rules file yet: the starting list applies (it excludes /tmp,
        // where this folder is) and nothing is written.
        let size = local_size(root, &|| false, &mut |_| {}).unwrap().unwrap();
        if root.starts_with("/tmp") {
            assert_eq!(size.files, 0);
        }
        assert!(!root.join(FILE_IGNORE_NAME).exists());
        fs::write(root.join(FILE_IGNORE_NAME), "skip\n*.tmp\n").unwrap();
        let size = local_size(root, &|| false, &mut |_| {}).unwrap().unwrap();
        assert_eq!(size, LocalSize { files: 2, bytes: 8 });
        assert_eq!(local_size(root, &|| true, &mut |_| {}).unwrap(), None);
        assert!(local_size(&root.join("missing"), &|| false, &mut |_| {}).is_err());
    }

    #[test]
    fn the_warning_counts_only_what_the_remote_lacks() {
        let mut estimate = Estimate {
            done: true,
            local_size: LocalSize {
                files: 10,
                bytes: 5_000,
            },
            remote_size: Some(FolderSize {
                count: 4,
                bytes: 2_000,
                sizeless: 0,
            }),
            free: Some(4_000),
            ..Estimate::default()
        };
        assert_eq!(estimate.warning(), None);
        estimate.free = Some(2_000);
        assert!(estimate.warning().unwrap().contains("3.0 KB"));
        estimate.free = None;
        assert_eq!(estimate.warning(), None);
        estimate.free = Some(0);
        estimate.done = false;
        assert_eq!(estimate.warning(), None);
    }
}
