//! XDG desktop integration: app-menu entry, login autostart, and which
//! binary those entries should launch.

use std::path::{Path, PathBuf};

/// The file desktop entries should launch.
///
/// From the portable `.run` this is the `.run` itself (exported by its stub
/// as `CELESTIAL_PORTABLE_EXE`), never the versioned extraction under
/// `~/.cache`, which is replaced on every upgrade.
pub fn resolve_binary_path() -> PathBuf {
    if let Some(exe) = std::env::var_os("CELESTIAL_PORTABLE_EXE").map(PathBuf::from) {
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

fn icon_path() -> PathBuf {
    data_home()
        .join("icons/hicolor/scalable/apps")
        .join(format!("{}.svg", crate::APP_ID))
}

/// Write the embedded SVG next to the user's other icons.
fn install_icon() -> Option<PathBuf> {
    const SVG: &[u8] = crate::ui::branding::LOGO_SVG;
    let path = icon_path();
    if std::fs::read(&path).ok().as_deref() == Some(SVG) {
        return Some(path);
    }
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

pub fn remove_icon() -> Result<Option<PathBuf>, String> {
    remove(icon_path())
}

/// Records that the menu entry was added automatically once, so a later
/// `uninstall` is not undone by the next start.
fn first_run_marker() -> PathBuf {
    data_home().join("celestial").join("menu-entry-added")
}

pub fn remember_first_run() -> Result<(), String> {
    let marker = first_run_marker();
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(marker, "").map_err(|e| e.to_string())
}

/// Add the app-menu entry the first time the portable launcher runs.
fn add_menu_entry_on_first_run(exec: &Path) {
    if first_run_marker().exists() {
        return;
    }
    if !menu_entry_path().exists() {
        match write_menu_entry(exec) {
            Ok(path) => tracing::info!(path = %path.display(), "added app-menu entry"),
            Err(e) => {
                tracing::warn!("could not add app-menu entry: {e}");
                return;
            }
        }
    }
    if let Err(e) = remember_first_run() {
        tracing::warn!("could not record the app-menu entry: {e}");
    }
}

/// The app ID used while the app was called Celeste.
const LEGACY_APP_ID: &str = "com.hunterwittenborn.Celeste";

/// Remove the menu entry, autostart entry and icon written under the former
/// name. Returns whether a former autostart entry existed.
pub fn remove_legacy_entries() -> bool {
    let entry = format!("{LEGACY_APP_ID}.desktop");
    let autostart = config_home().join("autostart").join(&entry);
    let had_autostart = autostart.is_file();
    for path in [
        data_home().join("applications").join(&entry),
        autostart,
        data_home()
            .join("icons/hicolor/scalable/apps")
            .join(format!("{LEGACY_APP_ID}.svg")),
    ] {
        match remove(path) {
            Ok(Some(path)) => tracing::info!(path = %path.display(), "removed former entry"),
            Ok(None) => {}
            Err(e) => tracing::warn!("could not remove former entry: {e}"),
        }
    }
    had_autostart
}

/// Replace entries written under the former name with current ones.
fn migrate_legacy_entries(exec: &Path) {
    let had_menu = data_home()
        .join("applications")
        .join(format!("{LEGACY_APP_ID}.desktop"))
        .is_file();
    let had_autostart = remove_legacy_entries();
    let mut writes = Vec::new();
    if had_menu {
        writes.push(menu_entry_path());
    }
    if had_autostart {
        writes.push(autostart_path());
    }
    for path in writes {
        if let Err(e) = write_entry(&path, exec) {
            tracing::warn!(path = %path.display(), "could not write desktop entry: {e}");
        }
    }
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

/// `major.minor.patch` from `celestial --version` output.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split_whitespace().last()?.split('.');
    let mut next = || parts.next()?.parse().ok();
    Some((next()?, next()?, next()?))
}

/// Overwrite the launcher a desktop entry points at with the portable `.run`
/// running now, unless that launcher reports a newer version. Otherwise an
/// older install keeps starting at login against a database a newer build
/// already migrated, and refuses to run.
fn replace_older_launcher(target: &Path, current: &Path) {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same(target, current) {
        return;
    }
    if let (Ok(a), Ok(b)) = (std::fs::read(target), std::fs::read(current)) {
        if a == b {
            return;
        }
    }
    let ours = parse_version(env!("CARGO_PKG_VERSION"));
    let theirs = std::process::Command::new(target)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| parse_version(&String::from_utf8_lossy(&o.stdout)));
    if let (Some(ours), Some(theirs)) = (ours, theirs) {
        if theirs > ours {
            return;
        }
    }
    // Copy beside it and rename: replacing a running executable in place
    // fails with "Text file busy".
    let staged = target.with_extension("new");
    let result = std::fs::copy(current, &staged)
        .and_then(|_| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        })
        .and_then(|_| std::fs::rename(&staged, target));
    match result {
        Ok(()) => tracing::info!(path = %target.display(), "replaced older installed launcher"),
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            tracing::warn!(path = %target.display(), "could not replace installed launcher: {e}");
        }
    }
}

/// Add the app-menu entry on the first run, and repoint existing entries
/// whose absolute launcher path no longer exists (the `.run` was moved or
/// renamed) at the portable launcher running now.
///
/// Existing icons are refreshed on any GUI launch. Launcher paths are only
/// updated from the portable bundle; autostart is never created here.
pub fn refresh_stale_entries() {
    // An upgrade can change the embedded logo while keeping the same launcher.
    if (menu_entry_path().is_file() || autostart_path().is_file()) && install_icon().is_none() {
        tracing::warn!("could not refresh the installed application icon");
    }
    if std::env::var_os("CELESTIAL_PORTABLE_EXE").is_none() {
        return;
    }
    let current = resolve_binary_path();
    migrate_legacy_entries(&current);
    add_menu_entry_on_first_run(&current);
    for path in [menu_entry_path(), autostart_path()] {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(target) = exec_target(&contents) else {
            continue;
        };
        // Bare names (`Exec=celestial`) resolve via PATH; leave them alone.
        let target = Path::new(&target);
        if !target.is_absolute() {
            continue;
        }
        if target.is_file() {
            replace_older_launcher(target, &current);
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
        let e = desktop_entry(Path::new("/opt/My App/celestial.run"), None);
        assert!(e.contains("Exec=\"/opt/My App/celestial.run\"\n"));
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
