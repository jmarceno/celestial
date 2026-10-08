//! Editable starting rules, with home/absolute paths anchored to the local side.

use super::localfs::Root;
use super::model::FILE_IGNORE_NAME;
use glob::{MatchOptions, Pattern};
use regex::Regex;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
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
        "~/go",
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
        "**/.angular/cache",
        "**/.sass-cache",
        "**/.eslintcache",
        "**/.stylelintcache",
        "builtin:rust-target",
        // Download caches; keep each tool's settings and installed user commands.
        "~/.m2/repository",
        "~/.nuget/packages",
        "~/.local/share/NuGet/v3-cache",
        "~/.local/share/NuGet/plugins-cache",
        "~/.swiftpm/cache",
        "~/Library/Caches",
        "~/Library/Developer/Xcode/DerivedData",
        "~/.dub/packages",
        "~/.hex/packages",
        "~/.cabal/packages",
        "~/.cabal/store",
        "~/.stack/pantry",
        "~/.konan/cache",
        "~/.konan/dependencies",
        // Python test environments and notebook checkpoints are reproducible.
        "**/.hypothesis",
        "**/.tox",
        "**/.nox",
        "**/.ipynb_checkpoints",
        // JavaScript/TypeScript dependencies, generated framework files and caches.
        "**/node_modules",
        "**/.next",
        "**/.nuxt",
        "**/.svelte-kit",
        "**/.vite",
        "**/.turbo",
        "**/.nyc_output",
        "**/.yarn/unplugged",
        "**/.yarn/install-state.gz",
        "**/.yarn/build-state.yml",
        "**/*.tsbuildinfo",
        // Shared native compiler caches cover C/C++, Fortran, Ada, ASM and COBOL.
        "**/CMakeFiles",
        "**/CMakeCache.txt",
        "**/cmake-build-*",
        "**/autom4te.cache",
        "**/.ninja_log",
        "**/.ninja_deps",
        "**/.deps",
        "**/.libs",
        r"regex:(^|/)(build|obj)/.*\.(o|ali|smod|dcu|ppu|class|gch|pch)$",
        // Maven/Gradle/Kotlin: skip generated subtrees, not every folder named build.
        "**/target/classes",
        "**/target/test-classes",
        "**/target/generated-sources",
        "**/target/generated-test-sources",
        "**/target/maven-status",
        "**/target/maven-archiver",
        "**/target/surefire-reports",
        "**/target/failsafe-reports",
        "**/.gradle/caches",
        "**/.gradle/[0-9]*",
        "**/.gradle/buildOutputCleanup",
        "**/.gradle/configuration-cache",
        "**/.gradle/checksums",
        "**/.gradle/vcs-1",
        "**/.kotlin/sessions",
        "**/.kotlin/errors",
        "**/.kotlin/daemon",
        "**/build/classes",
        "**/build/tmp",
        "**/build/generated",
        "**/build/intermediates",
        // .NET, Visual Basic and SQL projects: explicit build profiles and restore caches.
        "**/[Bb]in/[Dd]ebug",
        "**/[Bb]in/[Rr]elease",
        "**/[Oo]bj/[Dd]ebug",
        "**/[Oo]bj/[Rr]elease",
        "**/[Oo]bj/project.assets.json",
        "**/[Oo]bj/project.nuget.cache",
        "**/[Oo]bj/*.nuget.g.props",
        "**/[Oo]bj/*.nuget.g.targets",
        "**/[Oo]bj/*.nuget.dgspec.json",
        // Other recognizable tool outputs; preserve sources, databases and sessions.
        "**/*.identcache",
        "**/*.Rcheck",
        "**/.build",
        "**/.swiftpm/cache",
        "**/.phpunit.cache",
        "**/.phpunit.result.cache",
        "**/.php-cs-fixer.cache",
        "**/.phpcs-cache",
        "**/target/partial_parse.msgpack",
        "**/.dub/build",
        "**/duende_build",
        "**/_build",
        "**/.elixir_ls",
        "**/.stack-work",
        "**/dist-newstyle",
        "**/luac.out",
        "**/.godot",
        "**/.direnv",
        "/**/steamapps/common",
        "/**/steamapps/shadercache",
        "/**/steamapps/downloading",
        "/**/steamapps/temp",
        "/dev",
        "/proc",
        // Not all of /run: udisks mounts removable drives under /run/media.
        "/run/user",
        "/run/lock",
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
    let home = absolute_env("HOME");
    let xdg = |name| absolute_env(name).map(|path| rule_for(&path, home.as_deref()));
    if let Some(cache) = xdg("XDG_CACHE_HOME") {
        lines.push(cache);
    }
    if let Some(config) = xdg("XDG_CONFIG_HOME") {
        add_config_caches(&mut lines, &config);
    }
    if let Some(prefix) = xdg("XDG_DATA_HOME") {
        lines.push(format!("{prefix}/Trash"));
        lines.push(format!("{prefix}/pnpm/store"));
    }
    // Keep the list stable and omit duplicate entries from conventional XDG paths.
    let mut seen = HashSet::new();
    lines.retain(|line| seen.insert(line.clone()));
    lines
}

fn add_config_caches(lines: &mut Vec<String>, prefix: &str) {
    for cache in CONFIG_CACHES {
        lines.push(format!("{prefix}/*/**/{cache}"));
    }
}

/// A literal rule for `path`, written as `~/` when it is inside the home
/// folder so that conventional XDG locations repeat (and drop) a default.
fn rule_for(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => {
            format!("~/{}", Pattern::escape(&rel.to_string_lossy()))
        }
        _ => Pattern::escape(&path.to_string_lossy()),
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
        drop(file);
        let root = Root::open(local, None)?;
        match root.rename_noreplace(&name, FILE_IGNORE_NAME) {
            Err(err) if err.kind() != std::io::ErrorKind::AlreadyExists => {
                // Network and FUSE filesystems may lack renameat2. A hard
                // link also publishes the complete file without replacing.
                fs::hard_link(&temporary, &path).map_err(|link| match link.kind() {
                    std::io::ErrorKind::AlreadyExists => link,
                    _ => err,
                })
            }
            result => result,
        }
    })();
    let _ = fs::remove_file(temporary);
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

/// `.sync-exclude.lst.<pid>.<n>.tmp`, left behind if `initialize` is interrupted.
fn is_initialize_leftover(relative: &str) -> bool {
    relative
        .strip_prefix(FILE_IGNORE_NAME)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| rest.strip_suffix(".tmp"))
        .and_then(|rest| rest.split_once('.'))
        .is_some_and(|(pid, n)| {
            [pid, n]
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
}

pub struct Rules {
    root: PathBuf,
    relative: Vec<Pattern>,
    absolute: Vec<Pattern>,
    regexes: Vec<Regex>,
    /// An absolute rule covers the selected folder itself (or a parent).
    root_excluded: bool,
    rust_target: bool,
    remote_targets: BTreeSet<String>,
    local_targets: RefCell<HashMap<String, bool>>,
}

/// Use the same parser when saving and when synchronizing. Invalid rules
/// must be rejected before a working exclusions file is truncated.
pub fn validate(text: &str) -> Result<(), String> {
    Rules::parse(Path::new("/"), absolute_env("HOME").as_deref(), text).map(|_| ())
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
            regexes: Vec::new(),
            root_excluded: false,
            rust_target: false,
            remote_targets: BTreeSet::new(),
            local_targets: RefCell::new(HashMap::new()),
        };
        for line in text.lines().filter(|line| !line.is_empty()) {
            if line == "builtin:rust-target" {
                rules.rust_target = true;
                continue;
            }
            if let Some(expression) = line.strip_prefix("regex:") {
                if expression.is_empty() {
                    return Err("regex: requires a non-empty expression".into());
                }
                rules
                    .regexes
                    .push(Regex::new(expression).map_err(|err| format!("{line}: {err}"))?);
                continue;
            }
            if line.starts_with("builtin:") {
                return Err(format!("Unknown exclusion rule: {line}"));
            }
            let line = line.strip_prefix("glob:").unwrap_or(line);
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
        rules.root_excluded = local.ancestors().any(|path| rules.matches_absolute(path));
        Ok(rules)
    }

    fn matches_absolute(&self, path: &Path) -> bool {
        let options = MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::new()
        };
        self.absolute
            .iter()
            .any(|pattern| pattern.matches_path_with(path, options))
    }

    /// Build folders proven by the remote listing, which also exclude the
    /// matching local paths.
    pub fn remote_targets(&self) -> &BTreeSet<String> {
        &self.remote_targets
    }

    /// A recursive remote listing proves the structure even when the local
    /// cache is absent. The same exclusion is then applied to both sides.
    pub fn note_remote_targets(
        &mut self,
        remote_root: &str,
        items: &[crate::rclone::RcloneRemoteItem],
    ) {
        if !self.rust_target {
            return;
        }
        for item in items {
            let rel = if remote_root.is_empty() {
                Some(item.path.as_str())
            } else {
                item.path
                    .strip_prefix(remote_root)
                    .and_then(|tail| tail.strip_prefix('/'))
            };
            let Some(rel) = rel.filter(|rel| super::observe::valid_relative(rel)) else {
                continue;
            };
            let (parent, name) = rel.rsplit_once('/').unwrap_or(("", rel));
            if ((item.is_dir && matches!(name, "debug" | "release"))
                || (!item.is_dir && name == ".rustc_info.json"))
                && self.is_target_path(parent)
            {
                self.remote_targets.insert(parent.to_owned());
            }
        }
    }

    fn is_target_path(&self, rel: &str) -> bool {
        if rel.is_empty() {
            self.root.file_name().is_some_and(|name| name == "target")
        } else {
            rel.rsplit('/').next() == Some("target")
        }
    }

    pub fn matches_in(&self, relative: &str, local: &Root) -> bool {
        if self.matches(relative) {
            return true;
        }
        if !self.rust_target {
            return false;
        }
        let mut candidate = relative;
        loop {
            if self.is_target_path(candidate) {
                let known = self.local_targets.borrow().get(candidate).copied();
                let excluded = known.unwrap_or_else(|| {
                    let is_dir = candidate.is_empty()
                        || local
                            .lstat(candidate)
                            .is_ok_and(|meta| meta.is_some_and(|meta| meta.is_dir));
                    let excluded = is_dir
                        && ["debug", "release", ".rustc_info.json"].iter().any(|name| {
                            let rel = if candidate.is_empty() {
                                (*name).to_owned()
                            } else {
                                format!("{candidate}/{name}")
                            };
                            local.lstat(&rel).is_ok_and(|meta| {
                                meta.is_some_and(|meta| {
                                    if *name == ".rustc_info.json" {
                                        !meta.is_dir
                                    } else {
                                        meta.is_dir
                                    }
                                })
                            })
                        });
                    self.local_targets
                        .borrow_mut()
                        .insert(candidate.to_owned(), excluded);
                    excluded
                });
                if excluded {
                    return true;
                }
            }
            if candidate.is_empty() {
                return false;
            }
            candidate = candidate.rsplit_once('/').map_or("", |(parent, _)| parent);
        }
    }

    pub fn matches(&self, relative: &str) -> bool {
        if relative == FILE_IGNORE_NAME || is_initialize_leftover(relative) {
            return true;
        }
        // The selected root may itself be inside an excluded cache/temp folder.
        if self.root_excluded || self.remote_targets.contains("") {
            return true;
        }
        // Retain the historical semantics of relative user globs (e.g. *.tmp).
        // Absolute rules use the local name of each path on both sides.
        let mut path = relative;
        loop {
            if self.remote_targets.contains(path)
                || self.relative.iter().any(|pattern| pattern.matches(path))
                || self.regexes.iter().any(|regex| regex.is_match(path))
                || (!self.absolute.is_empty() && self.matches_absolute(&self.root.join(path)))
            {
                return true;
            }
            let Some((parent, _)) = path.rsplit_once('/') else {
                break;
            };
            path = parent;
        }
        false
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
            "go/pkg/mod/cache/download/module.zip",
            "go/bin/tool",
            "go/src/project/main.go",
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
            "Documents/go/main.go",
            "Projects/go/main.go",
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
        assert!(home_rules("/home").matches("alice/go/src/project/main.go"));
        assert!(home_rules("/").matches("home/alice/.cargo/registry/data"));
        assert!(home_rules("/home/alice/go").matches("src/project/main.go"));
        assert!(home_rules("/home/alice/.config").matches("google-chrome/Default/Cache/data"));
        assert!(home_rules("/home/alice/.cache").matches("pip/data"));
        assert!(!home_rules("/home/alice/Documents").matches(".cache/important.txt"));
        assert!(!home_rules("/home/bob").matches(".cache/important.txt"));
        assert!(!home_rules("/home/bob").matches("go/src/project/main.go"));
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
        // Removable drives are mounted under /run/media by udisks.
        assert!(!home_rules("/run/media/alice/USB").matches("Documents/report.txt"));
        assert!(!home_rules("/").matches("run/media/alice/USB/report.txt"));
        assert!(home_rules("/").matches("run/user/1000/gvfs/share/file"));
    }

    #[test]
    fn xdg_locations_inside_home_are_written_as_home_rules() {
        let home = Path::new("/home/alice[1]");
        assert_eq!(
            rule_for(Path::new("/home/alice[1]/.cache"), Some(home)),
            "~/.cache"
        );
        assert_eq!(
            rule_for(Path::new("/home/alice[1]/x[y]"), Some(home)),
            "~/x[[]y[]]"
        );
        assert_eq!(rule_for(Path::new("/srv/cache"), Some(home)), "/srv/cache");
        assert_eq!(rule_for(home, Some(home)), "/home/alice[[]1[]]");
        assert_eq!(rule_for(Path::new("/srv/cache"), None), "/srv/cache");
    }

    #[test]
    fn only_initialize_leftovers_are_protected() {
        let rules = Rules::parse(Path::new("/data"), None, "").unwrap();
        assert!(rules.matches(".sync-exclude.lst.123.0.tmp"));
        for rel in [
            ".sync-exclude.lst.notes.tmp",
            ".sync-exclude.lst.1.tmp",
            ".sync-exclude.lst..1.tmp",
            "nested/.sync-exclude.lst.123.0.tmp",
        ] {
            assert!(!rules.matches(rel), "user file excluded: {rel}");
        }
    }

    #[test]
    fn development_defaults_skip_generated_data_and_preserve_project_inputs() {
        let rules = home_rules("/home/alice");
        for path in [
            ".m2/repository/org/example/library.jar",
            ".nuget/packages/library/lib/compiled.dll",
            ".swiftpm/cache/repositories/checkouts",
            "Library/Developer/Xcode/DerivedData/App/Build/data",
            ".dub/packages/package/source/installed.d",
            ".hex/packages/hexpm/package.tar",
            ".cabal/store/ghc/package/lib.a",
            ".stack/pantry/hackage/index.tar",
            ".konan/dependencies/llvm/bin/clang",
            "Projects/python/.hypothesis/examples/data",
            "Projects/python/.tox/py313/lib/package.py",
            "Projects/notebook/.ipynb_checkpoints/report.ipynb",
            "Projects/web/node_modules/library/index.js",
            "Projects/web/.next/server/page.js",
            "Projects/web/.svelte-kit/generated/manifest.js",
            "Projects/web/.nuxt/manifest.js",
            "Projects/web/.vite/deps/library.js",
            "Projects/web/.turbo/cache/file",
            "Projects/web/tsconfig.tsbuildinfo",
            "Projects/native/build/CMakeFiles/app.dir/main.cpp.o",
            "Projects/native/build/CMakeCache.txt",
            "Projects/native/cmake-build-debug/app",
            "Projects/native/build/.ninja_deps",
            "Projects/native/autom4te.cache/output",
            "Projects/native/obj/main.o",
            "Projects/ada/obj/main.ali",
            "Projects/fortran/obj/module.smod",
            "Projects/pascal/build/unit.ppu",
            "Projects/delphi/build/unit.dcu",
            "Projects/delphi/Project.identcache",
            "Projects/java/target/classes/Main.class",
            "Projects/java/target/generated-sources/Generated.java",
            "Projects/kotlin/.gradle/caches/data",
            "Projects/kotlin/.kotlin/sessions/data",
            "Projects/kotlin/build/classes/Main.class",
            "Projects/dotnet/bin/Debug/net8.0/App.dll",
            "Projects/vb/Bin/Release/net8.0/App.dll",
            "Projects/sql/obj/project.assets.json",
            "Projects/sql/target/partial_parse.msgpack",
            "Projects/r/package.Rcheck/check.log",
            "Projects/swift/.build/debug/App",
            "Projects/php/.phpunit.cache/test-results",
            "Projects/php/.php-cs-fixer.cache",
            "Projects/d/.dub/build/debug/app.o",
            "Projects/duende/duende_build/generated.d",
            "Projects/elixir/_build/dev/lib/app/ebin/app.beam",
            "Projects/elixir/.elixir_ls/build/data",
            "Projects/haskell/.stack-work/dist/Main.o",
            "Projects/haskell/dist-newstyle/build/Main.o",
            "Projects/lua/luac.out",
            "Projects/godot/.godot/imported/image.ctex",
            "Projects/nix/.direnv/flake-profile",
        ] {
            assert!(
                rules.matches(path),
                "development cache not excluded: {path}"
            );
        }
        for path in [
            ".m2/settings.xml",
            ".nuget/NuGet/NuGet.Config",
            ".swiftpm/configuration/registries.json",
            ".dub/settings.json",
            ".hex/hex.config",
            ".cabal/config",
            ".cabal/bin/my-tool",
            ".stack/config.yaml",
            ".konan/konan.properties",
            "Projects/d/source/main.d",
            "Projects/duende/examples/main.du",
            "Projects/native/build/README.md",
            "Projects/native/obj/model.obj",
            "Projects/native/Makefile",
            "Projects/native/CMakeLists.txt",
            "Projects/native/vendor/library.so",
            "Projects/java/lib/library.jar",
            "Projects/java/pom.xml",
            "Projects/kotlin/build.gradle.kts",
            "Projects/kotlin/.gradle/gradle.properties",
            "Projects/kotlin/.gradle/init.d/custom.gradle",
            "Projects/kotlin/.kotlin/tool-settings.json",
            "Projects/dotnet/App.csproj",
            "Projects/dotnet/bin/important-tool",
            "Projects/vb/App.vbproj",
            "Projects/sql/queries/query.sql",
            "Projects/sql/database.sqlite-wal",
            "Projects/sql/database.sqlite-shm",
            "Projects/sql/backup.sql",
            "Projects/scratch/project.sb3",
            "Projects/r/.Rhistory",
            "Projects/r/.RData",
            "Projects/r/.Renviron",
            "Projects/elixir/mix.lock",
            "Projects/elixir/deps/local-patch/lib/app.ex",
            "Projects/python/.venv/custom-source.py",
            "Projects/python/notebook.ipynb",
            "Projects/swift/Package.resolved",
            "Projects/godot/scene.tscn",
            "Projects/godot/player.gd.uid",
            "Projects/nix/.envrc",
            "Projects/nix/flake.lock",
            "Music/song.mod",
            ".local/bin/installed-command",
        ] {
            assert!(!rules.matches(path), "project input excluded: {path}");
        }
        // The logical home is backed by a disposable filesystem for the
        // structural probe, so /tmp itself does not trigger a home-path rule.
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("Projects/rust/target/debug")).unwrap();
        fs::create_dir_all(temp.path().join("Documents/target")).unwrap();
        let root = Root::open(temp.path(), None).unwrap();
        assert!(rules.matches_in("Projects/rust/target/debug/app", &root));
        assert!(!rules.matches_in("Documents/target/important.txt", &root));
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

    #[test]
    fn regexes_match_relative_paths_and_excluded_parents() {
        let rules = Rules::parse(
            Path::new("/home/alice/Documents"),
            None,
            r"regex:(^|/)target/(debug|release)(/|$)
regex:(?i)^cache files$
*.bak
glob:regex:literal[0-9].txt",
        )
        .unwrap();
        for rel in [
            "target/debug/app",
            "Projects/app/target/release/deps/lib.rlib",
            "CACHE FILES/data",
            "nested/file.bak",
            "regex:literal1.txt",
        ] {
            assert!(rules.matches(rel), "regex/glob missed {rel}");
        }
        for rel in [
            "Projects/app/target/source.rs",
            "Projects/app/target/debugger/readme",
            "other/CACHE FILES/data",
            "target/release-notes.txt",
            "important.txt",
        ] {
            assert!(!rules.matches(rel), "regex/glob matched {rel}");
        }
    }

    #[test]
    fn invalid_regex_saves_keep_the_previous_rules_intact() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().to_str().unwrap();
        super::super::model::write_exclusions(local, "*.bak").unwrap();
        for invalid in ["regex:[", "regex:", "builtin:unknown"] {
            assert!(super::super::model::write_exclusions(local, invalid).is_err());
            assert_eq!(
                fs::read_to_string(temp.path().join(FILE_IGNORE_NAME)).unwrap(),
                "*.bak"
            );
        }
        super::super::model::write_exclusions(local, r"regex:\.tmp$").unwrap();
        assert!(
            Rules::load(temp.path())
                .unwrap()
                .matches("nested/cache.tmp")
        );
    }

    #[test]
    fn rust_target_requires_a_directory_with_real_build_markers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        fs::create_dir_all(path.join("project/target/debug")).unwrap();
        fs::create_dir_all(path.join("release-only/target/release")).unwrap();
        fs::create_dir_all(path.join("cross/target/custom-triple/release")).unwrap();
        fs::write(path.join("cross/target/.rustc_info.json"), "{}").unwrap();
        fs::create_dir_all(path.join("documents/target")).unwrap();
        fs::write(
            path.join("documents/target/debug"),
            "a document, not a directory",
        )
        .unwrap();
        fs::create_dir_all(path.join("targeted/debug")).unwrap();
        fs::create_dir_all(path.join("linked-marker/target")).unwrap();
        std::os::unix::fs::symlink(
            path.join("project/target/debug"),
            path.join("linked-marker/target/debug"),
        )
        .unwrap();
        std::os::unix::fs::symlink(path.join("project/target"), path.join("linked-target"))
            .unwrap();
        let root = Root::open(path, None).unwrap();
        let rules = Rules::parse(path, None, "builtin:rust-target").unwrap();
        for rel in [
            "project/target",
            "project/target/debug/app",
            "project/target/other-cache",
            "release-only/target/release/app",
            "cross/target/custom-triple/release/app",
        ] {
            assert!(
                rules.matches_in(rel, &root),
                "build folder not excluded: {rel}"
            );
        }
        for rel in [
            "project/src/main.rs",
            "documents/target/debug",
            "documents/target/readme",
            "targeted/debug/app",
            "linked-marker/target/debug/app",
            "linked-target/debug/app",
        ] {
            assert!(
                !rules.matches_in(rel, &root),
                "non-build folder excluded: {rel}"
            );
        }
        // Each new pass rechecks the structure instead of retaining an old match.
        fs::remove_dir(path.join("project/target/debug")).unwrap();
        let rules = Rules::parse(path, None, "builtin:rust-target").unwrap();
        assert!(!rules.matches_in("project/target/readme", &root));
    }

    #[test]
    fn a_selected_target_root_is_detected_and_the_rule_is_optional() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("target");
        fs::create_dir_all(path.join("release")).unwrap();
        let root = Root::open(&path, None).unwrap();
        let rules = Rules::parse(&path, None, "builtin:rust-target").unwrap();
        assert!(rules.matches_in("release/app", &root));
        let rules = Rules::parse(&path, None, "").unwrap();
        assert!(!rules.matches_in("release/app", &root));
    }
}
