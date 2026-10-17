use futures::future::Future;
use std::path::PathBuf;

/// The ID of the app, used for desktop entries, the icon and the tray item.
/// Entries written under the former ID are migrated by `platform::desktop`.
pub static APP_ID: &str = "com.unboundmark.Celestial";

pub static APP_NAME: &str = "Celestial";

/// Get the value out of a future.
pub fn await_future<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Format a directory with the user's home directory replaced with '~'.
pub fn fmt_home(dir: &str) -> String {
    let home_dir = home_dir();
    match dir.strip_prefix(&home_dir) {
        Some(string) if !home_dir.is_empty() => "~".to_string() + string,
        _ => dir.to_string(),
    }
}

/// Celestial's config directory (`$XDG_CONFIG_HOME/celestial`, usually
/// `~/.config/celestial`). `CELESTIAL_CONFIG_DIR` overrides it for tests.
pub fn get_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CELESTIAL_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    config_base().join("celestial")
}

fn config_base() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(base) => PathBuf::from(base),
        None => PathBuf::from(home_dir()).join(".config"),
    }
}

/// The database file inside [`get_config_dir`].
pub const DATABASE_NAME: &str = "celestial.db";

/// Move the configuration and history kept under the app's former name,
/// Celeste (`celeste/` and `celeste.db`), to the current names. Nothing is
/// moved when the current files already exist.
pub fn migrate_legacy_config() -> std::io::Result<()> {
    let legacy = std::env::var_os("CELESTIAL_CONFIG_DIR")
        .is_none()
        .then(|| config_base().join("celeste"));
    move_legacy_config(legacy.as_deref(), &get_config_dir())
}

fn move_legacy_config(
    legacy: Option<&std::path::Path>,
    config: &std::path::Path,
) -> std::io::Result<()> {
    if let Some(legacy) = legacy.filter(|legacy| legacy.is_dir()) {
        if !config.exists() {
            std::fs::rename(legacy, config)?;
        }
    }
    let legacy_db = config.join("celeste.db");
    let db = config.join(DATABASE_NAME);
    if legacy_db.is_file() && !db.exists() {
        std::fs::rename(legacy_db, db)?;
    }
    Ok(())
}

/// Celestial's log, next to its configuration.
pub fn log_path() -> PathBuf {
    get_config_dir().join("celestial.log")
}

/// The log is moved aside at start once it is this large.
const LOG_LIMIT: u64 = 20 * 1024 * 1024;

/// Send stderr (Celestial's log and the messages of Qt and rclone) to the
/// log file when it is not a terminal, so a session started from the app
/// menu leaves a trace, even of a crash. One older log is kept.
///
/// `CELESTIAL_LOG_STDERR` (set to anything non-empty) keeps stderr as it is.
/// A caller that redirected stderr on purpose — the portable smoke test, a
/// wrapper capturing output — would otherwise see an empty stream and read
/// the log file instead.
pub fn log_to_file() {
    use std::io::IsTerminal;
    use std::os::fd::AsRawFd;
    if std::io::stderr().is_terminal() {
        return;
    }
    if std::env::var_os("CELESTIAL_LOG_STDERR").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    if let Err(err) = nix::unistd::dup2(file.as_raw_fd(), 2) {
        tracing::warn!("could not write the log to {}: {err}", path.display());
    }
}

/// `$XDG_RUNTIME_DIR/celestial` for the single-instance socket.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uid())));
    base.join("celestial")
}

fn uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

/// Map a remote item onto the local sync directory.
///
/// Rclone paths are relative to the remote root. Stripping `remote_root` can
/// leave a leading slash, and joining that with another slash produces
/// `local//name`. That path does not match the one recorded while walking the
/// local tree, so the same file is synced twice and can crash the pass.
///
/// Returns [`None`] when `remote_item_path` is not inside `remote_root`.
pub fn local_path_for_remote(
    local_root: &str,
    remote_root: &str,
    remote_item_path: &str,
) -> Option<String> {
    // Remote paths are relative; reject traversal before joining the local root.
    if remote_item_path.starts_with('/') || remote_item_path.contains('\0') {
        return None;
    }
    if remote_item_path
        .split('/')
        .any(|part| part == ".." || part == ".")
    {
        return None;
    }
    let relative = if remote_root.is_empty() {
        remote_item_path
    } else {
        remote_item_path.strip_prefix(&format!("{remote_root}/"))?
    };
    if relative.is_empty() {
        return None;
    }
    let root = local_root.trim_end_matches('/');
    Some(format!("{root}/{relative}"))
}

/// Strip the slashes from the beginning and end of a string.
pub fn strip_slashes(string: &str) -> String {
    let stripped_prefix = match string.strip_prefix('/') {
        Some(string) => string.to_string(),
        None => string.to_string(),
    };

    match stripped_prefix.strip_suffix('/') {
        Some(string) => string.to_string(),
        None => stripped_prefix,
    }
}

#[cfg(test)]
mod tests {
    use super::{DATABASE_NAME, local_path_for_remote, move_legacy_config};
    use std::fs;

    #[test]
    fn moves_former_config_and_database() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("celeste");
        let config = temp.path().join("celestial");
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("rclone.conf"), "remotes").unwrap();
        fs::write(legacy.join("celeste.db"), "history").unwrap();
        move_legacy_config(Some(&legacy), &config).unwrap();
        assert!(!legacy.exists());
        assert_eq!(
            fs::read_to_string(config.join("rclone.conf")).unwrap(),
            "remotes"
        );
        assert_eq!(
            fs::read_to_string(config.join(DATABASE_NAME)).unwrap(),
            "history"
        );
    }

    #[test]
    fn keeps_existing_config_over_former_one() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("celeste");
        let config = temp.path().join("celestial");
        fs::create_dir(&legacy).unwrap();
        fs::write(legacy.join("celeste.db"), "old").unwrap();
        fs::create_dir(&config).unwrap();
        fs::write(config.join(DATABASE_NAME), "current").unwrap();
        move_legacy_config(Some(&legacy), &config).unwrap();
        assert!(legacy.join("celeste.db").exists());
        assert_eq!(
            fs::read_to_string(config.join(DATABASE_NAME)).unwrap(),
            "current"
        );
    }

    #[test]
    fn joins_remote_path_without_double_slash() {
        assert_eq!(
            local_path_for_remote("/home/user/Google", "docs", "docs/AsciiArt.txt").as_deref(),
            Some("/home/user/Google/AsciiArt.txt")
        );
    }

    #[test]
    fn joins_root_remote() {
        assert_eq!(
            local_path_for_remote("/home/user/Google", "", "AsciiArt.txt").as_deref(),
            Some("/home/user/Google/AsciiArt.txt")
        );
    }

    #[test]
    fn rejects_traversal_and_root_entries() {
        for path in [
            "docs/../outside",
            "docs/./file",
            "docs",
            "docs/",
            "/absolute",
        ] {
            assert_eq!(local_path_for_remote("/local", "docs", path), None);
        }
        assert_eq!(
            local_path_for_remote("/", "", "file").as_deref(),
            Some("/file")
        );
    }

    #[test]
    fn rejects_path_outside_sync_root() {
        assert_eq!(
            local_path_for_remote("/home/user/Google", "docs", "other/file.txt"),
            None
        );
    }
}
