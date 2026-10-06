use adw::glib::{self, MainContext};
use futures::future::Future;
use std::path::PathBuf;

/// The ID of the app.
pub static APP_ID: &str = "com.hunterwittenborn.Celeste";

/// Get the value out of a future.
pub fn await_future<F: Future>(future: F) -> F::Output {
    futures::executor::block_on(future)
}

/// Run a closure in the background so that the UI can keep running.
pub fn run_in_background<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(f: F) -> T {
    MainContext::default().block_on(blocking::unblock(f))
}

/// Format a directory with the user's home directory replaced with '~'.
pub fn fmt_home(dir: &str) -> String {
    let home_dir = glib::home_dir().into_os_string().into_string().unwrap();

    match dir.strip_prefix(&home_dir) {
        Some(string) => "~".to_string() + string,
        None => dir.to_string(),
    }
}

/// Get the user's config directory.
pub fn get_config_dir() -> PathBuf {
    let mut config_dir = glib::user_config_dir();
    config_dir.push("celeste");
    config_dir
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
