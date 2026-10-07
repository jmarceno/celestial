//! Local file operations relative to an open, verified pair root.
//!
//! Every path is resolved one component at a time with `O_NOFOLLOW`, so a
//! symbolic link or a swapped folder anywhere inside the pair makes the
//! operation fail instead of reaching outside it. The root itself is checked
//! against the (device, inode) it had when it was observed.

use super::observe::LocalMeta;
use nix::errno::Errno;
use nix::fcntl::{OFlag, RenameFlags, openat, renameat2};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat, mkdirat};
use nix::unistd::{UnlinkatFlags, fsync, unlinkat};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

fn os(err: Errno) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

fn owned(fd: std::os::fd::RawFd) -> OwnedFd {
    // SAFETY: `fd` was just returned by a successful `openat` and is owned
    // by nothing else.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

const DIR_FLAGS: OFlag = OFlag::O_RDONLY
    .union(OFlag::O_DIRECTORY)
    .union(OFlag::O_NOFOLLOW)
    .union(OFlag::O_CLOEXEC);

pub fn meta_from_stat(stat: &FileStat) -> LocalMeta {
    let is_dir = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR;
    LocalMeta {
        is_dir,
        size: if is_dir {
            0
        } else {
            u64::try_from(stat.st_size).unwrap_or(0)
        },
        mtime: super::observe::Stamp::new(stat.st_mtime, stat.st_mtime_nsec),
        ctime: super::observe::Stamp::new(stat.st_ctime, stat.st_ctime_nsec),
        inode: stat.st_ino,
        device: stat.st_dev,
    }
}

fn is_regular_or_dir(stat: &FileStat) -> bool {
    let kind = SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT;
    kind == SFlag::S_IFDIR || kind == SFlag::S_IFREG
}

/// An open pair root.
pub struct Root {
    fd: OwnedFd,
    path: PathBuf,
}

impl Root {
    /// Open the root, refusing a symbolic link and, when given, a folder
    /// other than the one observed.
    pub fn open(path: &Path, identity: Option<(u64, u64)>) -> io::Result<Self> {
        let fd = owned(openat(libc_cwd(), path, DIR_FLAGS, Mode::empty()).map_err(os)?);
        let stat = fstat(fd.as_raw_fd()).map_err(os)?;
        if identity.is_some_and(|identity| identity != (stat.st_dev, stat.st_ino)) {
            return Err(io::Error::other(
                "The sync folder was replaced while it was being synchronized",
            ));
        }
        Ok(Self {
            fd,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> io::Result<(u64, u64)> {
        let stat = fstat(self.fd.as_raw_fd()).map_err(os)?;
        Ok((stat.st_dev, stat.st_ino))
    }

    /// Open the folder containing `rel` and return it with the last name.
    /// Fails when any folder on the way is missing, not a folder, or a link.
    fn parent(&self, rel: &str) -> io::Result<(Option<OwnedFd>, String)> {
        let mut parts: Vec<&str> = rel.split('/').filter(|part| !part.is_empty()).collect();
        let name = parts
            .pop()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty path"))?;
        if name == "." || name == ".." || parts.iter().any(|part| *part == "." || *part == "..") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path leaves the sync folder",
            ));
        }
        let mut current: Option<OwnedFd> = None;
        for part in parts {
            let base = current
                .as_ref()
                .map_or(self.fd.as_raw_fd(), AsRawFd::as_raw_fd);
            let next = openat(base, part, DIR_FLAGS, Mode::empty()).map_err(|err| {
                // A link where a folder should be is refused, not treated as
                // a missing folder.
                let link = matches!(err, Errno::ELOOP | Errno::ENOTDIR)
                    && fstatat(base, part, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(
                        |stat| {
                            SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT
                                == SFlag::S_IFLNK
                        },
                    );
                if link {
                    io::Error::other("Symbolic links inside a synced folder are not followed")
                } else {
                    os(err)
                }
            })?;
            current = Some(owned(next));
        }
        Ok((current, name.to_owned()))
    }

    fn dirfd(&self, parent: &Option<OwnedFd>) -> std::os::fd::RawFd {
        parent
            .as_ref()
            .map_or(self.fd.as_raw_fd(), AsRawFd::as_raw_fd)
    }

    /// Metadata of `rel` without following links; `None` when it (or a
    /// folder above it) does not exist.
    pub fn lstat(&self, rel: &str) -> io::Result<Option<LocalMeta>> {
        let (parent, name) = match self.parent(rel) {
            Ok(found) => found,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(err) => return Err(err),
        };
        match fstatat(
            self.dirfd(&parent),
            name.as_str(),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if is_regular_or_dir(&stat) => Ok(Some(meta_from_stat(&stat))),
            Ok(_) => Err(io::Error::other(
                "Only regular files and folders are synchronized",
            )),
            Err(Errno::ENOENT | Errno::ENOTDIR) => Ok(None),
            Err(err) => Err(os(err)),
        }
    }

    /// Rename within the pair. `NOREPLACE` and `EXCHANGE` are required; a
    /// filesystem without them pauses the item rather than falling back to
    /// delete-then-copy.
    pub fn rename(&self, from: &str, to: &str, flags: RenameFlags) -> io::Result<()> {
        let (from_parent, from_name) = self.parent(from)?;
        let (to_parent, to_name) = self.parent(to)?;
        renameat2(
            Some(self.dirfd(&from_parent)),
            from_name.as_str(),
            Some(self.dirfd(&to_parent)),
            to_name.as_str(),
            flags,
        )
        .map_err(|err| match err {
            Errno::EINVAL | Errno::ENOSYS | Errno::EOPNOTSUPP => io::Error::other(
                "This filesystem cannot replace files atomically; the item is paused",
            ),
            other => os(other),
        })?;
        self.sync_parent(&from_parent);
        self.sync_parent(&to_parent);
        Ok(())
    }

    pub fn rename_noreplace(&self, from: &str, to: &str) -> io::Result<()> {
        self.rename(from, to, RenameFlags::RENAME_NOREPLACE)
    }

    pub fn exchange(&self, first: &str, second: &str) -> io::Result<()> {
        self.rename(first, second, RenameFlags::RENAME_EXCHANGE)
    }

    pub fn mkdir(&self, rel: &str) -> io::Result<()> {
        let (parent, name) = self.parent(rel)?;
        match mkdirat(
            self.dirfd(&parent),
            name.as_str(),
            Mode::from_bits_truncate(0o777),
        ) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(err) => return Err(os(err)),
        }
        self.sync_parent(&parent);
        Ok(())
    }

    /// Create every missing folder of `rel` (inclusive).
    pub fn mkdir_all(&self, rel: &str) -> io::Result<()> {
        let mut path = String::new();
        for part in rel.split('/').filter(|part| !part.is_empty()) {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(part);
            self.mkdir(&path)?;
        }
        Ok(())
    }

    /// Remove an empty folder; fails when it has contents.
    pub fn rmdir(&self, rel: &str) -> io::Result<()> {
        let (parent, name) = self.parent(rel)?;
        unlinkat(
            Some(self.dirfd(&parent)),
            name.as_str(),
            UnlinkatFlags::RemoveDir,
        )
        .map_err(os)?;
        self.sync_parent(&parent);
        Ok(())
    }

    pub fn unlink(&self, rel: &str) -> io::Result<()> {
        let (parent, name) = self.parent(rel)?;
        unlinkat(
            Some(self.dirfd(&parent)),
            name.as_str(),
            UnlinkatFlags::NoRemoveDir,
        )
        .map_err(os)
    }

    /// Flush a file's contents to disk.
    pub fn sync_file(&self, rel: &str) -> io::Result<()> {
        let (parent, name) = self.parent(rel)?;
        let fd = owned(
            openat(
                self.dirfd(&parent),
                name.as_str(),
                OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(os)?,
        );
        fsync(fd.as_raw_fd()).map_err(os)
    }

    fn sync_parent(&self, parent: &Option<OwnedFd>) {
        // Best effort: the rename itself already happened.
        let _ = fsync(self.dirfd(parent));
    }
}

fn libc_cwd() -> std::os::fd::RawFd {
    nix::libc::AT_FDCWD
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn links_inside_the_pair_are_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let root = Root::open(&root, None).unwrap();
        assert!(root.lstat("link/secret").is_err());
        assert!(root.rename_noreplace("link/secret", "stolen").is_err());
        assert!(fs::read_to_string(outside.join("secret")).is_ok());
        assert!(root.lstat("../outside/secret").is_err());
    }

    #[test]
    fn a_replaced_root_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("root");
        fs::create_dir(&path).unwrap();
        let identity = Root::open(&path, None).unwrap().identity().unwrap();
        fs::remove_dir(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(Root::open(&path, Some(identity)).is_err());
    }

    #[test]
    fn missing_parents_are_absence_not_errors() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("file"), "x").unwrap();
        let root = Root::open(temp.path(), None).unwrap();
        assert_eq!(root.lstat("missing/child").unwrap(), None);
        assert_eq!(root.lstat("file/child").unwrap(), None);
        root.mkdir_all("a/b/c").unwrap();
        assert!(root.lstat("a/b/c").unwrap().unwrap().is_dir);
        assert!(root.rmdir("a").is_err());
    }
}
