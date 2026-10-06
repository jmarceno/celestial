use futures::future::Future;
use std::path::PathBuf;

/// The ID of the app. Kept stable so existing desktop entries and the tray
/// item stay the same item the session already knows.
pub static APP_ID: &str = "com.hunterwittenborn.Celeste";

pub static APP_NAME: &str = "Celeste";

/// Get the value out of a future.
pub fn await_future<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// Run a closure away from the GUI thread.
///
/// Callers of rclone already run on the sync worker. The closure runs inline
/// there so librclone stays on that one thread.
pub fn run_in_background<T, F: FnOnce() -> T>(f: F) -> T {
    f()
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

/// Celeste's config directory (`$XDG_CONFIG_HOME/celeste`, usually
/// `~/.config/celeste`). `CELESTE_CONFIG_DIR` overrides it for tests.
pub fn get_config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CELESTE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(base).join("celeste");
    }
    PathBuf::from(home_dir()).join(".config").join("celeste")
}

/// `$XDG_RUNTIME_DIR/celeste` for the single-instance socket.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", uid())));
    base.join("celeste")
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
    let relative = if remote_root.is_empty() {
        remote_item_path
    } else if remote_item_path == remote_root {
        ""
    } else {
        let prefix = format!("{remote_root}/");
        remote_item_path.strip_prefix(&prefix)?
    };
    let relative = relative.trim_matches('/');
    let local_root = local_root.trim_end_matches('/');
    if relative.is_empty() {
        Some(local_root.to_owned())
    } else {
        Some(format!("{local_root}/{relative}"))
    }
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

/// Macro to get the title of a window.
#[macro_export]
macro_rules! get_title {
    ($($arg:tt)*) => {
        tr::tr!($($arg)*) + " - Celeste"
    }
}

pub use crate::get_title;

#[cfg(test)]
mod tests {
    use super::local_path_for_remote;

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
    fn rejects_path_outside_sync_root() {
        assert_eq!(
            local_path_for_remote("/home/user/Google", "docs", "other/file.txt"),
            None
        );
    }
}
