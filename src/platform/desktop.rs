//! XDG desktop integration: app-menu entry, login autostart, and which
//! binary those entries should launch.

use std::path::{Path, PathBuf};

/// The file desktop entries should launch.
///
/// From the portable `.run` this is the `.run` itself (exported by its stub
/// as `CELESTE_PORTABLE_EXE`), never the versioned extraction under
/// `~/.cache`, which is replaced on every upgrade.
pub fn resolve_binary_path() -> PathBuf {
    if let Some(exe) = std::env::var_os("CELESTE_PORTABLE_EXE").map(PathBuf::from) {
        if exe.is_file() {
            return exe;
        }
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(crate::APP_ID))
}

fn data_home() -> PathBuf {
    if let Some(v) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".local/share"))
}

fn config_home() -> PathBuf {
    if let Some(v) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    directories::BaseDirs::new()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".config"))
}

fn entry_file_name() -> String {
    format!("{}.desktop", crate::APP_ID)
}

pub fn menu_entry_path() -> PathBuf {
    data_home().join("applications").join(entry_file_name())
}

pub fn autostart_path() -> PathBuf {
    config_home().join("autostart").join(entry_file_name())
}

/// Quote for a desktop-entry `Exec=` key.
fn quote_exec(path: &Path) -> String {
    let s = path.to_string_lossy();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A desktop entry launching `exec` with no arguments (= the GUI).
///
/// The icon is an absolute path to the copy installed by
/// [`install_icon`], so it works without an icon-theme install.
pub fn desktop_entry(exec: &Path, icon: Option<&Path>) -> String {
    let mut s = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={name}\n\
         Comment=Folder sync for the cloud providers rclone supports\n\
         Exec={exec}\n\
         Terminal=false\n\
         Categories=FileTools;FileTransfer;Utility;\n",
        name = crate::APP_NAME,
        exec = quote_exec(exec),
    );
    if let Some(icon) = icon {
        s.push_str(&format!("Icon={}\n", icon.display()));
    }
    s
}

/// Write the embedded SVG next to the user's other icons.
fn install_icon() -> Option<PathBuf> {
    const SVG: &[u8] = include_bytes!("../../assets/com.hunterwittenborn.Celeste-regular.svg");
    let path = data_home()
        .join("icons/hicolor/scalable/apps")
        .join(format!("{}.svg", crate::APP_ID));
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, SVG).ok()?;
    Some(path)
}

fn write_entry(path: &Path, exec: &Path) -> Result<PathBuf, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let icon = install_icon();
    std::fs::write(path, desktop_entry(exec, icon.as_deref())).map_err(|e| e.to_string())?;
    Ok(path.to_path_buf())
}

pub fn write_menu_entry(exec: &Path) -> Result<PathBuf, String> {
    write_entry(&menu_entry_path(), exec)
}

pub fn write_autostart(exec: &Path) -> Result<PathBuf, String> {
    write_entry(&autostart_path(), exec)
}

fn remove(path: PathBuf) -> Result<Option<PathBuf>, String> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Some(path)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn remove_menu_entry() -> Result<Option<PathBuf>, String> {
    remove(menu_entry_path())
}

pub fn remove_autostart() -> Result<Option<PathBuf>, String> {
    remove(autostart_path())
}

/// First word of `Exec=` with quotes stripped, skipping an `env VAR=…` prefix.
fn exec_target(contents: &str) -> Option<String> {
    let line = contents.lines().find_map(|l| l.strip_prefix("Exec="))?;
    let mut words = line.split_whitespace().map(|w| w.trim_matches('"'));
    let mut w = words.next()?;
    if w == "env" {
        w = words.find(|w| !w.contains('='))?;
    }
    Some(w.to_string())
}

/// Repoint existing entries whose absolute launcher path no longer exists
/// (the `.run` was moved or renamed) at the portable launcher running now.
///
/// Only runs from the portable bundle, so a dev `cargo run` never hijacks
/// the user's entries, and never creates entries that did not exist.
pub fn refresh_stale_entries() {
    if std::env::var_os("CELESTE_PORTABLE_EXE").is_none() {
        return;
    }
    let current = resolve_binary_path();
    for path in [menu_entry_path(), autostart_path()] {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(target) = exec_target(&contents) else {
            continue;
        };
        // Bare names (`Exec=skeleton`) resolve via PATH; leave them alone.
        let target = Path::new(&target);
        if !target.is_absolute() || target.is_file() {
            continue;
        }
        match write_entry(&path, &current) {
            Ok(_) => tracing::info!(path = %path.display(), "repointed stale desktop entry"),
            Err(e) => {
                tracing::warn!(path = %path.display(), "could not refresh desktop entry: {e}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_quotes_exec_and_has_no_args() {
        let e = desktop_entry(Path::new("/opt/My App/celeste.run"), None);
        assert!(e.contains("Exec=\"/opt/My App/celeste.run\"\n"));
        assert!(!e.contains("Icon="));
    }

    #[test]
    fn exec_target_parsing() {
        assert_eq!(
            exec_target("Exec=env A=1 /a/b --x\n").as_deref(),
            Some("/a/b")
        );
        assert_eq!(exec_target("Exec=\"/b/c\"\n").as_deref(), Some("/b/c"));
        assert_eq!(exec_target("Name=x\n"), None);
    }
}
