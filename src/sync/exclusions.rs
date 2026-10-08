//! Editable starting rules, with home/absolute paths anchored to the local side.

use super::model::FILE_IGNORE_NAME;
use glob::{MatchOptions, Pattern};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const CONFIG_CACHES: &[&str] = &[
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "GrShaderCache",
    "ShaderCache",
    "CachedData",
];

pub fn defaults() -> Vec<String> {
    let mut lines: Vec<String> = [
        "~/.cache",
        "~/.gvfs",
        "~/.thumbnails",
        "~/.nv/GLCache",
        "~/.nv/ComputeCache",
        "~/.local/share/Trash",
        "~/.cargo/registry",
        "~/.cargo/git/db",
        "~/.cargo/git/checkouts",
        "~/.npm/_cacache",
        "~/.npm/_logs",
        "~/.npm/_npx",
        "~/.yarn/berry/cache",
        "~/.local/share/pnpm/store",
        "~/.pnpm-store",
        "~/.gradle/caches",
        "~/.gradle/daemon",
        "~/.gradle/wrapper/dists",
        "~/.composer/cache",
        "~/.var/app/*/cache",
        "~/.var/app/*/.cache",
        "~/snap/*/*/.cache",
        "**/__pycache__",
        "**/.pytest_cache",
        "**/.mypy_cache",
        "**/.ruff_cache",
        "**/.pytype",
        "**/.pyre",
        "**/.parcel-cache",
        "**/.next/cache",
        "**/.angular/cache",
        "**/.sass-cache",
        "**/.eslintcache",
        "**/.stylelintcache",
        "/**/steamapps/common",
        "/**/steamapps/shadercache",
        "/**/steamapps/downloading",
        "/**/steamapps/temp",
        "/dev",
        "/proc",
        "/run",
        "/sys",
        "/tmp",
        "/var/tmp",
        "/var/cache",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    for prefix in ["~/.config", "~/.var/app/*/config", "~/snap/*/*/.config"] {
        add_config_caches(&mut lines, prefix);
    }
    for prefix in [
        "~/.mozilla/firefox/*",
        "~/.librewolf/*",
        "~/.var/app/*/.mozilla/firefox/*",
        "~/.var/app/*/.librewolf/*",
        "~/snap/*/*/.mozilla/firefox/*",
    ] {
        for cache in ["cache2", "startupCache", "shader-cache"] {
            lines.push(format!("{prefix}/{cache}"));
        }
    }
    // Client caches complement the library rules, keeping userdata and saves.
    for prefix in [
        "~/.local/share/Steam",
        "~/.steam/steam",
        "~/.steam/root",
        "~/.steam/debian-installation",
        "~/.var/app/com.valvesoftware.Steam/data/Steam",
        "~/.var/app/com.valvesoftware.Steam/.local/share/Steam",
        "~/.var/app/com.valvesoftware.Steam/.steam/root",
        "~/snap/steam/common/.local/share/Steam",
    ] {
        for cache in ["appcache", "depotcache"] {
            lines.push(format!("{prefix}/{cache}"));
        }
    }
    // Honor relocated XDG storage as well as the conventional home layout.
    if let Some(cache) = absolute_env("XDG_CACHE_HOME") {
        lines.push(Pattern::escape(&cache.to_string_lossy()));
    }
    if let Some(config) = absolute_env("XDG_CONFIG_HOME") {
        add_config_caches(&mut lines, &Pattern::escape(&config.to_string_lossy()));
    }
    if let Some(data) = absolute_env("XDG_DATA_HOME") {
        let prefix = Pattern::escape(&data.to_string_lossy());
        lines.push(format!("{prefix}/Trash"));
        lines.push(format!("{prefix}/pnpm/store"));
    }
    // Keep the list stable and omit duplicate entries from conventional XDG paths.
    let mut seen = std::collections::HashSet::new();
    lines.retain(|line| seen.insert(line.clone()));
    lines
}

fn add_config_caches(lines: &mut Vec<String>, prefix: &str) {
    for cache in CONFIG_CACHES {
        lines.push(format!("{prefix}/*/**/{cache}"));
    }
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

/// Create the initial list once. An existing list, including an empty one,
/// is the user's choice and is never augmented or reset.
pub fn initialize(local: &Path) -> Result<(), String> {
    let path = local.join(FILE_IGNORE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.to_string()),
    }
    let body = defaults().join("\n") + "\n";
    // Publish complete rules atomically without replacing another creator's list.
    // The temporary file is also excluded if the process is interrupted.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "{FILE_IGNORE_NAME}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let temporary = local.join(&name);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|err| err.to_string())?;
    let result = (|| {
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        super::localfs::Root::open(local, None)?.rename_noreplace(&name, FILE_IGNORE_NAME)
    })();
    let _ = fs::remove_file(temporary);
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

pub struct Rules {
    root: PathBuf,
    relative: Vec<Pattern>,
    absolute: Vec<Pattern>,
}

impl Rules {
    pub fn load(local: &Path) -> Result<Self, String> {
        initialize(local)?;
        let path = local.join(FILE_IGNORE_NAME);
        if !fs::metadata(&path)
            .map_err(|err| err.to_string())?
            .is_file()
        {
            return Err("Exclusion rules are not a regular file".into());
        }
        let _lock =
            file_lock::FileLock::lock(&path, true, file_lock::FileOptions::new().read(true))
                .map_err(|err| err.to_string())?;
        let text = fs::read_to_string(path).map_err(|err| err.to_string())?;
        Self::parse(local, absolute_env("HOME").as_deref(), &text)
    }

    fn parse(local: &Path, home: Option<&Path>, text: &str) -> Result<Self, String> {
        let mut rules = Self {
            root: local.to_path_buf(),
            relative: Vec::new(),
            absolute: Vec::new(),
        };
        for line in text.lines().filter(|line| !line.is_empty()) {
            let expanded = if let Some(suffix) = line.strip_prefix("~/") {
                let home = home.ok_or("Cannot expand ~/ in exclusions: HOME is unavailable")?;
                format!("{}/{suffix}", Pattern::escape(&home.to_string_lossy()))
            } else {
                line.to_owned()
            };
            let pattern = Pattern::new(&expanded).map_err(|err| format!("{line}: {err}"))?;
            if expanded.starts_with('/') {
                rules.absolute.push(pattern);
            } else {
                rules.relative.push(pattern);
            }
        }
        Ok(rules)
    }

    pub fn matches(&self, relative: &str) -> bool {
        if relative == FILE_IGNORE_NAME
            || (relative.starts_with(&format!("{FILE_IGNORE_NAME}.")) && relative.ends_with(".tmp"))
        {
            return true;
        }
        // Retain the historical semantics of relative user globs (e.g. *.tmp).
        let mut path = relative;
        loop {
            if self.relative.iter().any(|pattern| pattern.matches(path)) {
                return true;
            }
            let Some((parent, _)) = path.rsplit_once('/') else {
                break;
            };
            path = parent;
        }
        // Apply the local absolute name to both sides. Check parents too when
        // the selected root is itself inside an excluded cache/temp directory.
        let absolute = self.root.join(relative);
        let options = MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::new()
        };
        absolute.ancestors().any(|path| {
            self.absolute
                .iter()
                .any(|pattern| pattern.matches_path_with(path, options))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home_rules(root: &str) -> Rules {
        Rules::parse(
            Path::new(root),
            Some(Path::new("/home/alice")),
            &defaults().join("\n"),
        )
        .unwrap()
    }

    #[test]
    fn home_defaults_skip_caches_and_preserve_restore_data() {
        let rules = home_rules("/home/alice");
        for path in [
            ".cache/pip/wheels/cached.whl",
            ".cache/mozilla/firefox/profile/cache2/entries/cached",
            ".cargo/registry/src/crate/lib.rs",
            ".cargo/git/checkouts/crate/lib.rs",
            ".npm/_cacache/content-v2/cache",
            ".local/share/pnpm/store/v3/cache",
            ".gradle/caches/modules-2/cached.jar",
            ".config/google-chrome/Default/Cache/Cache_Data/data",
            ".config/BraveSoftware/Brave-Browser/Profile 1/Code Cache/js/cache",
            ".config/microsoft-edge/Default/GPUCache/data",
            ".config/discord/Cache/data",
            ".config/Code/CachedData/compiled-js",
            ".mozilla/firefox/profile/cache2/entries/data",
            ".var/app/org.mozilla.firefox/cache/data",
            ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser/Default/Cache/data",
            "snap/chromium/common/.cache/data",
            "snap/firefox/common/.mozilla/firefox/profile/startupCache/data",
            "Projects/app/__pycache__/module.pyc",
            "Projects/app/.pytest_cache/v/cache/nodeids",
            "Projects/web/.next/cache/webpack/data",
            ".local/share/Steam/steamapps/shadercache/123/cache",
            ".local/share/Steam/steamapps/common/Game/game.exe",
            "Games/SteamLibrary/steamapps/common/Game/assets.pak",
            ".local/share/Trash/files/removed.txt",
        ] {
            assert!(rules.matches(path), "cache not excluded: {path}");
        }
        for path in [
            "Documents/report.txt",
            ".ssh/id_ed25519",
            ".Private/encrypted-document",
            ".local/bin/my-tool",
            ".local/share/app/database.sqlite",
            ".local/state/app/state.json",
            ".config/google-chrome/Default/Bookmarks",
            ".config/BraveSoftware/Brave-Browser/Default/Login Data",
            ".config/google-chrome/Default/Local Storage/leveldb/data",
            ".mozilla/firefox/profile/places.sqlite",
            ".mozilla/firefox/profile/logins.json",
            ".var/app/org.mozilla.firefox/.mozilla/firefox/profile/places.sqlite",
            ".cargo/config.toml",
            ".cargo/credentials.toml",
            ".cargo/bin/my-tool",
            ".gradle/gradle.properties",
            ".local/share/Steam/userdata/123/456/remote/save.dat",
            ".local/share/Steam/steamapps/compatdata/123/pfx/drive_c/users/steamuser/save.dat",
            "Projects/app/.git/objects/hash",
            "Projects/app/target/important.dat",
            "Projects/app/.venv/lib/package.py",
            "Documents/Cache/important.txt",
        ] {
            assert!(!rules.matches(path), "restore data excluded: {path}");
        }
    }

    #[test]
    fn anchored_rules_work_for_parent_nested_and_unrelated_roots() {
        assert!(home_rules("/home").matches("alice/.cache/pip/data"));
        assert!(home_rules("/").matches("home/alice/.cargo/registry/data"));
        assert!(home_rules("/home/alice/.config").matches("google-chrome/Default/Cache/data"));
        assert!(home_rules("/home/alice/.cache").matches("pip/data"));
        assert!(!home_rules("/home/alice/Documents").matches(".cache/important.txt"));
        assert!(!home_rules("/home/bob").matches(".cache/important.txt"));
        assert!(!home_rules("/home/alice").matches("tmp/important.txt"));
        assert!(home_rules("/").matches("tmp/session/data"));
        assert!(home_rules("/mnt/games/SteamLibrary").matches("steamapps/common/Game/game.exe"));
        assert!(home_rules("/mnt/games/SteamLibrary").matches("steamapps/shadercache/123/cache"));
        assert!(
            home_rules("/mnt/games/SteamLibrary").matches("steamapps/downloading/123/game.exe")
        );
        assert!(home_rules("/mnt/games/SteamLibrary").matches("steamapps/temp/123/partial"));
        assert!(home_rules("/mnt/games/SteamLibrary/steamapps/common/Game").matches("game.exe"));
        assert!(
            !home_rules("/mnt/games/SteamLibrary").matches("steamapps/compatdata/123/save.dat")
        );
    }

    #[test]
    fn relative_rules_keep_existing_glob_semantics_and_spaces() {
        let rules = Rules::parse(
            Path::new("/home/alice/Documents"),
            Some(Path::new("/home/alice")),
            "*.tmp\ncache\nCode Cache\n",
        )
        .unwrap();
        assert!(rules.matches("nested/file.tmp"));
        assert!(rules.matches("cache/nested/file.txt"));
        assert!(rules.matches("Code Cache/js/file"));
        assert!(!rules.matches("nested/cache/file.txt"));
        assert!(!rules.matches("important.tmp.txt"));
    }

    #[test]
    fn home_names_with_glob_characters_are_literal() {
        let rules = Rules::parse(
            Path::new("/home/alice[1]"),
            Some(Path::new("/home/alice[1]")),
            "~/.cache\n",
        )
        .unwrap();
        assert!(rules.matches(".cache/data"));
        assert!(Rules::parse(Path::new("/home"), None, "~/.cache").is_err());
    }

    #[test]
    fn defaults_are_created_once_and_user_edits_survive_reloads() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        initialize(root).unwrap();
        let path = root.join(FILE_IGNORE_NAME);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            defaults().join("\n") + "\n"
        );
        assert_eq!(
            super::super::model::read_exclusions(root.to_str().unwrap()),
            defaults()
        );
        super::super::model::write_exclusions(root.to_str().unwrap(), "*.tmp").unwrap();
        initialize(root).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "*.tmp");
        let rules = Rules::load(root).unwrap();
        assert!(rules.matches("temp.tmp"));
        assert!(!rules.matches("__pycache__/module.pyc"));
        super::super::model::write_exclusions(root.to_str().unwrap(), "").unwrap();
        initialize(root).unwrap();
        let rules = Rules::load(root).unwrap();
        assert!(!rules.matches("nested/temp.tmp"));
        assert!(rules.matches(FILE_IGNORE_NAME));
        assert!(rules.matches(".sync-exclude.lst.123.0.tmp"));
        assert!(super::super::model::read_exclusions(root.to_str().unwrap()).is_empty());
    }

    #[test]
    fn existing_invalid_or_non_file_rules_are_never_reset() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FILE_IGNORE_NAME);
        fs::write(&path, "[invalid").unwrap();
        assert!(Rules::load(temp.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "[invalid");
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(Rules::load(temp.path()).is_err());
    }
}
