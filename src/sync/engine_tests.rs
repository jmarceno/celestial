//! All rclone operations here target disposable local directories, never cloud accounts.
use super::*;
use crate::entities::{RemotesActiveModel, SyncItemsActiveModel};
use crate::sync::model::FILE_IGNORE_NAME;
use crate::sync::observe;
use crate::sync::recovery;
use crate::sync::testing::{Fixture, TestHooks, permissions_apply, set_time};
use sea_orm::Database;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;

fn rows(f: &Fixture) -> Vec<crate::entities::SyncItemsModel> {
    util::await_future(SyncItemsEntity::find().all(&f.db)).unwrap()
}

fn item_states(f: &Fixture) -> std::collections::BTreeMap<String, store::ItemState> {
    store::load_item_states(&f.db, f.dir.id).unwrap()
}

fn sql(f: &Fixture, text: &str) {
    store::exec(&f.db, text, vec![]).unwrap();
}

#[test]
fn deleting_one_remote_file_preserves_neighbors_and_other_folders() {
    let fixture = Fixture::new();
    fs::write(fixture.remote_root.join("sandbox/victim.txt"), "victim").unwrap();
    fs::write(fixture.remote_root.join("sandbox/neighbor.txt"), "neighbor").unwrap();
    fs::write(fixture.remote_root.join("outside.txt"), "outside").unwrap();
    rclone::sync::delete(&fixture.remote.name, "sandbox/victim.txt").unwrap();
    assert_eq!(
        fs::read_to_string(fixture.remote_root.join("outside.txt")).unwrap(),
        "outside"
    );
    assert_eq!(fixture.read_remote("neighbor.txt"), "neighbor");
    assert!(!fixture.remote_file("victim.txt").exists());
}

#[test]
fn failed_upload_preserves_existing_remote_copy() {
    if !permissions_apply() {
        return;
    }
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    let sandbox = f.remote_root.join("sandbox");
    fs::set_permissions(&sandbox, fs::Permissions::from_mode(0o555)).unwrap();
    f.pass();
    fs::set_permissions(&sandbox, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(f.read_remote("file.txt"), "original");
    assert_eq!(item_states(&f)["file.txt"].state, store::RETRYING);
    // The retry succeeds once the destination is writable again.
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "local edit");
}

#[test]
fn failed_download_preserves_existing_local_copy() {
    if !permissions_apply() {
        return;
    }
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    fs::set_permissions(&f.local, fs::Permissions::from_mode(0o555)).unwrap();
    f.pass();
    fs::set_permissions(&f.local, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(f.read_local("file.txt"), "original");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn source_changed_before_the_transfer_is_retried_not_copied() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "first edit").unwrap();
    let path = f.local.join("file.txt");
    f.pass_with(|_| {
        let _ = fs::write(&path, "second edit!");
    });
    assert_eq!(f.read_remote("file.txt"), "original");
    assert_eq!(item_states(&f)["file.txt"].state, store::RETRYING);
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "second edit!");
}

#[test]
fn local_child_edit_syncs_when_parent_timestamp_is_unchanged() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("nested")).unwrap();
    f.seed("nested/file.txt");
    let parent_time = fs::metadata(f.local.join("nested"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(f.local.join("nested/file.txt"), "edited locally").unwrap();
    set_time(&f.local.join("nested/file.txt"), 1_700_000_100);
    File::open(f.local.join("nested"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(parent_time))
        .unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("nested/file.txt"), "edited locally");
}

#[test]
fn remote_child_edit_syncs_when_parent_timestamp_is_unchanged() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("nested")).unwrap();
    f.seed("nested/file.txt");
    let parent = f.remote_root.join("sandbox/nested");
    let parent_time = fs::metadata(&parent).unwrap().modified().unwrap();
    fs::write(parent.join("file.txt"), "edited remotely").unwrap();
    set_time(&parent.join("file.txt"), 1_700_000_100);
    File::open(&parent)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(parent_time))
        .unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("nested/file.txt"), "edited remotely");
}

#[test]
fn unchanged_nonroot_pair_is_detected_as_clean() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.assert_clean();
}

#[test]
fn new_download_records_actual_local_metadata() {
    let mut f = Fixture::new();
    fs::write(f.remote_file("file.txt"), "remote only").unwrap();
    set_time(&f.remote_file("file.txt"), 1_700_000_100);
    f.pass();
    f.assert_no_errors();
    let baseline = &store::load_baselines(&f.db, &f.dir).unwrap()["file.txt"];
    let actual = fs::metadata(f.local.join("file.txt")).unwrap();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(baseline.local.unwrap().inode, actual.ino());
    assert_eq!(baseline.local_secs, actual.mtime());
    assert!(baseline.verified());
    f.assert_clean();
}

#[test]
fn stop_pair_is_scoped_to_its_remote() {
    let mut f = Fixture::new();
    let other = util::await_future(
        RemotesActiveModel {
            name: ActiveValue::Set("other".into()),
            ..Default::default()
        }
        .insert(&f.db),
    )
    .unwrap();
    let other_dir = util::await_future(
        SyncDirsActiveModel {
            remote_id: ActiveValue::Set(other.id),
            local_path: ActiveValue::Set(f.dir.local_path.clone()),
            remote_path: ActiveValue::Set(f.dir.remote_path.clone()),
            ..Default::default()
        }
        .insert(&f.db),
    )
    .unwrap();
    let mut queue = VecDeque::from([Command::StopDir {
        remote: other.name,
        local: f.dir.local_path.clone(),
        remote_path: f.dir.remote_path.clone(),
    }]);
    apply_deletes(&mut f.model, &f.db, &mut queue, &f.shared);
    assert!(
        util::await_future(SyncDirsEntity::find_by_id(f.dir.id).one(&f.db))
            .unwrap()
            .is_some()
    );
    assert!(
        util::await_future(SyncDirsEntity::find_by_id(other_dir.id).one(&f.db))
            .unwrap()
            .is_none()
    );
}

#[test]
fn unavailable_local_root_preserves_remote_files() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_dir_all(&f.local).unwrap();
    f.pass();
    assert_eq!(f.read_remote("file.txt"), "original");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn unavailable_remote_root_preserves_local_files() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_dir_all(f.remote_root.join("sandbox")).unwrap();
    f.pass();
    assert_eq!(f.read_local("file.txt"), "original");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn remote_folder_deletion_preserves_changed_local_children() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("nested")).unwrap();
    f.seed("nested/file.txt");
    f.seed("neighbor.txt");
    fs::remove_dir_all(f.remote_root.join("sandbox/nested")).unwrap();
    fs::write(f.local.join("nested/file.txt"), "unsynced edit").unwrap();
    set_time(&f.local.join("nested/file.txt"), 1_700_000_100);
    f.pass();
    assert_eq!(conflicts(&f), 1);
    // The edit survives on both sides.
    assert_eq!(f.read_local("nested/file.txt"), "unsynced edit");
    assert_eq!(f.read_remote("nested/file.txt"), "unsynced edit");
}

#[test]
fn local_folder_deletion_preserves_changed_remote_children() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("nested")).unwrap();
    f.seed("nested/file.txt");
    f.seed("nested/other.txt");
    f.seed("neighbor.txt");
    fs::remove_dir_all(f.local.join("nested")).unwrap();
    fs::write(f.remote_file("nested/file.txt"), "unsynced remote edit").unwrap();
    set_time(&f.remote_file("nested/file.txt"), 1_700_000_100);
    f.pass();
    assert_eq!(conflicts(&f), 1);
    assert_eq!(f.read_remote("nested/file.txt"), "unsynced remote edit");
    assert_eq!(f.read_local("nested/file.txt"), "unsynced remote edit");
    // The unchanged sibling follows the local deletion.
    assert!(!f.remote_file("nested/other.txt").exists());
}

#[test]
fn unchanged_folder_deletion_propagates_in_both_directions() {
    for delete_local in [false, true] {
        let mut f = Fixture::new();
        fs::create_dir(f.local.join("nested")).unwrap();
        f.seed("nested/file.txt");
        f.seed("neighbor.txt");
        if delete_local {
            fs::remove_dir_all(f.local.join("nested")).unwrap();
        } else {
            fs::remove_dir_all(f.remote_root.join("sandbox/nested")).unwrap();
        }
        f.pass();
        f.assert_no_errors();
        assert!(!f.local.join("nested").exists());
        assert!(!f.remote_root.join("sandbox/nested").exists());
        assert_eq!(f.read_local("neighbor.txt"), "original");
        assert_eq!(f.read_remote("neighbor.txt"), "original");
        f.assert_clean();
    }
}

#[test]
fn exclusions_apply_on_both_sides_of_nonroot_pair() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "*.tmp\n").unwrap();
    fs::write(f.local.join("local.tmp"), "excluded locally").unwrap();
    fs::write(f.remote_file("remote.tmp"), "excluded remotely").unwrap();
    f.seed("file.txt");
    assert!(!f.local.join("remote.tmp").exists());
    assert!(!f.remote_file("local.tmp").exists());
    assert!(!f.remote_file(FILE_IGNORE_NAME).exists());
    f.assert_clean();
}

#[test]
fn default_exclusions_skip_both_sides_and_removing_them_enables_sync() {
    let mut f = Fixture::new();
    fs::remove_file(f.local.join(FILE_IGNORE_NAME)).unwrap();
    crate::sync::exclusions::initialize(&f.local).unwrap();
    // Simulate this disposable root as the user's home without changing the
    // process environment. /tmp itself is intentionally excluded in production.
    let text = fs::read_to_string(f.local.join(FILE_IGNORE_NAME)).unwrap();
    let home = glob::Pattern::escape(f.local.to_str().unwrap());
    let text = text
        .lines()
        .filter(|line| *line != "/tmp")
        .map(|line| {
            line.strip_prefix("~/")
                .map_or_else(|| line.to_owned(), |tail| format!("{home}/{tail}"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    model::write_exclusions(f.local.to_str().unwrap(), &text).unwrap();
    for (path, content) in [
        (".cache/pip/local.whl", "cached"),
        (".cargo/registry/src/local.rs", "cached"),
        (
            ".config/BraveSoftware/Brave-Browser/Default/Code Cache/js/cache",
            "cached",
        ),
        ("Projects/app/__pycache__/module.pyc", "cached"),
        ("go/pkg/mod/cache/download/module.zip", "cached"),
        ("Projects/rust/target/debug/app", "cached"),
        ("Projects/rust/target/other-cache", "cached"),
        ("Projects/web/node_modules/package/index.js", "cached"),
        ("Projects/d/.dub/build/debug/app.o", "cached"),
        ("Projects/elixir/_build/dev/app.beam", "cached"),
        ("Projects/haskell/.stack-work/dist/Main.o", "cached"),
        ("Projects/native/obj/main.o", "cached"),
        ("Projects/web/tsconfig.tsbuildinfo", "cached"),
        (
            "Games/SteamLibrary/steamapps/common/Game/game.exe",
            "downloadable",
        ),
        (".local/share/app/database.sqlite", "keep"),
        (".config/google-chrome/Default/Bookmarks", "keep"),
        (".cargo/credentials.toml", "keep"),
        (
            "Games/SteamLibrary/steamapps/compatdata/123/save.dat",
            "keep",
        ),
        (".local/share/Steam/userdata/123/save.dat", "keep"),
        (".Private/document", "keep"),
        ("Projects/d/source/main.d", "keep"),
        ("Projects/native/build/README.md", "keep"),
        ("Projects/sql/database.sqlite-wal", "keep"),
        ("Projects/r/.RData", "keep"),
        ("Models/model.obj", "keep"),
        ("Music/song.mod", "keep"),
    ] {
        let target = f.local.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, content).unwrap();
    }
    for path in [
        ".cache/pip/remote.whl",
        ".var/app/org.mozilla.firefox/cache/remote-cache",
        "Games/SteamLibrary/steamapps/common/RemoteGame/game.exe",
        "go/bin/remote-tool",
        "Projects/remote-rust/target/release/app",
        "Projects/kotlin/.gradle/caches/remote-cache",
        "Projects/godot/.godot/imported/remote.ctex",
        "Projects/duende/duende_build/generated.d",
        "Projects/dotnet/obj/Debug/net8.0/remote.dll",
    ] {
        let target = f.remote_file(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, "remote cache/game").unwrap();
    }
    f.pass();
    for path in [
        ".cache/pip/local.whl",
        ".cargo/registry/src/local.rs",
        ".config/BraveSoftware/Brave-Browser/Default/Code Cache/js/cache",
        "Projects/app/__pycache__/module.pyc",
        "go/pkg/mod/cache/download/module.zip",
        "Projects/rust/target/debug/app",
        "Projects/rust/target/other-cache",
        "Projects/web/node_modules/package/index.js",
        "Projects/d/.dub/build/debug/app.o",
        "Projects/elixir/_build/dev/app.beam",
        "Projects/haskell/.stack-work/dist/Main.o",
        "Projects/native/obj/main.o",
        "Projects/web/tsconfig.tsbuildinfo",
        "Games/SteamLibrary/steamapps/common/Game/game.exe",
        FILE_IGNORE_NAME,
    ] {
        assert!(
            !f.remote_file(path).exists(),
            "excluded file uploaded: {path}"
        );
    }
    for path in [
        ".cache/pip/remote.whl",
        ".var/app/org.mozilla.firefox/cache/remote-cache",
        "Games/SteamLibrary/steamapps/common/RemoteGame/game.exe",
        "go/bin/remote-tool",
        "Projects/remote-rust/target/release/app",
        "Projects/kotlin/.gradle/caches/remote-cache",
        "Projects/godot/.godot/imported/remote.ctex",
        "Projects/duende/duende_build/generated.d",
        "Projects/dotnet/obj/Debug/net8.0/remote.dll",
    ] {
        assert!(
            !f.local.join(path).exists(),
            "excluded file downloaded: {path}"
        );
    }
    for path in [
        ".local/share/app/database.sqlite",
        ".config/google-chrome/Default/Bookmarks",
        ".cargo/credentials.toml",
        "Games/SteamLibrary/steamapps/compatdata/123/save.dat",
        ".local/share/Steam/userdata/123/save.dat",
        ".Private/document",
        "Projects/d/source/main.d",
        "Projects/native/build/README.md",
        "Projects/sql/database.sqlite-wal",
        "Projects/r/.RData",
        "Models/model.obj",
        "Music/song.mod",
    ] {
        assert_eq!(f.read_remote(path), "keep");
    }
    f.assert_clean();
    model::write_exclusions(f.local.to_str().unwrap(), "").unwrap();
    f.pass();
    assert_eq!(f.read_remote(".cache/pip/local.whl"), "cached");
    assert_eq!(f.read_local(".cache/pip/remote.whl"), "remote cache/game");
    assert_eq!(
        f.read_remote("go/pkg/mod/cache/download/module.zip"),
        "cached"
    );
    assert_eq!(f.read_remote("Projects/rust/target/other-cache"), "cached");
    assert_eq!(
        f.read_remote("Projects/elixir/_build/dev/app.beam"),
        "cached"
    );
    assert_eq!(
        f.read_local("Projects/remote-rust/target/release/app"),
        "remote cache/game"
    );
    assert_eq!(
        f.read_local("Projects/duende/duende_build/generated.d"),
        "remote cache/game"
    );
    assert_eq!(
        f.read_remote("Games/SteamLibrary/steamapps/common/Game/game.exe"),
        "downloadable"
    );
    assert_eq!(
        f.read_local("Games/SteamLibrary/steamapps/common/RemoteGame/game.exe"),
        "remote cache/game"
    );
    assert!(model::read_exclusions(f.local.to_str().unwrap()).is_empty());
    f.assert_clean();
}

#[test]
fn invalid_exclusions_pause_transfers() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "[invalid").unwrap();
    fs::write(f.local.join("file.txt"), "keep local").unwrap();
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert!(!f.remote_file("file.txt").exists());
}

#[test]
fn regex_exclusions_apply_to_both_sides_and_keep_globs_working() {
    let mut f = Fixture::new();
    model::write_exclusions(
        f.local.to_str().unwrap(),
        "regex:(^|/)target/(debug|release)(/|$)\n*.bak",
    )
    .unwrap();
    for (remote, rel) in [
        (false, "project/target/debug/local-app"),
        (true, "project/target/release/remote-app"),
        (false, "notes.bak"),
        (true, "remote-notes.bak"),
        (false, "project/target/source.rs"),
        (true, "project/src/main.rs"),
    ] {
        let path = if remote {
            f.remote_file(rel)
        } else {
            f.local.join(rel)
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "contents").unwrap();
    }
    f.pass();
    assert!(!f.remote_file("project/target/debug/local-app").exists());
    assert!(!f.local.join("project/target/release/remote-app").exists());
    assert!(!f.remote_file("notes.bak").exists());
    assert!(!f.local.join("remote-notes.bak").exists());
    assert_eq!(f.read_remote("project/target/source.rs"), "contents");
    assert_eq!(f.read_local("project/src/main.rs"), "contents");
    f.assert_clean();
}

#[test]
fn invalid_regex_from_disk_pauses_synchronization() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "regex:[").unwrap();
    fs::write(f.local.join("file.txt"), "keep local").unwrap();
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert!(!f.remote_file("file.txt").exists());
    assert_eq!(f.read_local("file.txt"), "keep local");
}

#[test]
fn rust_build_folders_on_either_side_are_skipped_and_plain_targets_are_kept() {
    let mut f = Fixture::new();
    model::write_exclusions(f.local.to_str().unwrap(), "builtin:rust-target").unwrap();
    for (remote, rel) in [
        (false, "project/target/debug/app"),
        (false, "project/target/release/app"),
        (false, "project/target/.rustc_info.json"),
        (false, "project/src/main.rs"),
        (false, "documents/target/readme.txt"),
        (true, "remote-project/target/release/app"),
        (true, "remote-project/src/main.rs"),
        (true, "cross/target/.rustc_info.json"),
        (true, "cross/target/custom-triple/release/app"),
        (true, "remote-documents/target/readme.txt"),
    ] {
        let path = if remote {
            f.remote_file(rel)
        } else {
            f.local.join(rel)
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "contents").unwrap();
    }
    f.pass();
    assert!(!f.remote_file("project/target").exists());
    assert!(!f.local.join("remote-project/target").exists());
    assert!(!f.local.join("cross/target").exists());
    assert_eq!(f.read_remote("project/src/main.rs"), "contents");
    assert_eq!(f.read_local("remote-project/src/main.rs"), "contents");
    assert_eq!(f.read_remote("documents/target/readme.txt"), "contents");
    assert_eq!(
        f.read_local("remote-documents/target/readme.txt"),
        "contents"
    );
    f.assert_clean();
    model::write_exclusions(f.local.to_str().unwrap(), "").unwrap();
    f.pass();
    assert_eq!(f.read_remote("project/target/release/app"), "contents");
    assert_eq!(
        f.read_local("remote-project/target/release/app"),
        "contents"
    );
    assert_eq!(
        f.read_local("cross/target/custom-triple/release/app"),
        "contents"
    );
    f.assert_clean();
}

#[test]
fn newly_detected_remote_build_structure_filters_a_cached_local_tree() {
    let mut f = Fixture::new();
    fs::create_dir_all(f.local.join("project/target")).unwrap();
    f.seed("project/target/readme.txt");
    f.assert_clean();
    let cached = f.last.as_mut().unwrap().local_tree.take().unwrap();
    fs::create_dir_all(f.remote_file("project/target/release")).unwrap();
    fs::write(f.remote_file("project/target/release/app"), "remote binary").unwrap();
    model::write_exclusions(f.local.to_str().unwrap(), "builtin:rust-target").unwrap();
    let mut hooks = TestHooks {
        calls: 0,
        on_checkpoint: None,
    };
    let report = pass::run(&f.pair(), &mut hooks, Some(cached)).unwrap();
    assert_eq!(report.actions, 0);
    // The cached walk predates the remote build folder, so it is walked again.
    assert!(report.stats.walked);
    assert!(!f.local.join("project/target/release").exists());
    assert_eq!(f.read_local("project/target/readme.txt"), "original");
    assert_eq!(f.read_remote("project/target/readme.txt"), "original");
    let cached = report.local_tree.unwrap();
    assert!(
        !cached
            .items
            .keys()
            .any(|rel| rel.starts_with("project/target"))
    );
    // Same remote structure: the cached tree is reused as is.
    let report = pass::run(&f.pair(), &mut hooks, Some(cached)).unwrap();
    assert_eq!(report.actions, 0);
    assert!(!report.stats.walked);
    let cached = report.local_tree.unwrap();
    // Once the remote build folder disappears, the local folder is included
    // again. The cached tree skipped it, so using it would look like the
    // local copy was deleted and remove the remote one.
    fs::remove_dir_all(f.remote_file("project/target/release")).unwrap();
    let report = pass::run(&f.pair(), &mut hooks, Some(cached)).unwrap();
    assert!(report.stats.walked);
    assert_eq!(f.read_local("project/target/readme.txt"), "original");
    assert_eq!(f.read_remote("project/target/readme.txt"), "original");
    f.assert_clean();
}

#[test]
fn symlink_inside_local_pair_cannot_overwrite_outside_file() {
    let mut f = Fixture::new();
    let outside = f.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file.txt"), "outside data").unwrap();
    std::os::unix::fs::symlink(&outside, f.local.join("linked")).unwrap();
    fs::create_dir(f.remote_root.join("sandbox/linked")).unwrap();
    fs::write(f.remote_file("linked/file.txt"), "remote data").unwrap();
    f.pass();
    assert_eq!(
        fs::read_to_string(outside.join("file.txt")).unwrap(),
        "outside data"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn database_read_failure_cannot_turn_tracked_edit_into_initial_sync() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_100);
    sql(&f, "DROP TABLE sync_items");
    f.pass();
    assert_eq!(f.read_remote("file.txt"), "original");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn database_write_failure_is_reported() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_100);
    sql(&f, "PRAGMA query_only = ON");
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert_eq!(rows(&f)[0].last_local_timestamp, 1_700_000_000);
}

#[test]
fn same_paths_on_other_remote_do_not_supply_this_pairs_baseline() {
    let mut f = Fixture::new();
    let other = util::await_future(
        RemotesActiveModel {
            name: ActiveValue::Set("other".into()),
            ..Default::default()
        }
        .insert(&f.db),
    )
    .unwrap();
    let other_dir = util::await_future(
        SyncDirsActiveModel {
            remote_id: ActiveValue::Set(other.id),
            local_path: ActiveValue::Set(f.dir.local_path.clone()),
            remote_path: ActiveValue::Set(f.dir.remote_path.clone()),
            ..Default::default()
        }
        .insert(&f.db),
    )
    .unwrap();
    util::await_future(
        SyncItemsActiveModel {
            sync_dir_id: ActiveValue::Set(other_dir.id),
            local_path: ActiveValue::Set(f.local.join("file.txt").to_str().unwrap().into()),
            remote_path: ActiveValue::Set("sandbox/file.txt".into()),
            last_local_timestamp: ActiveValue::Set(1_800_000_000),
            last_remote_timestamp: ActiveValue::Set(1_800_000_000),
            ..Default::default()
        }
        .insert(&f.db),
    )
    .unwrap();
    f.seed("file.txt");
    let all = rows(&f);
    assert_eq!(all.len(), 2);
    let find = |id| all.iter().find(|row| row.sync_dir_id == id).unwrap();
    assert_eq!(find(other_dir.id).last_local_timestamp, 1_800_000_000);
    assert_eq!(find(f.dir.id).last_local_timestamp, 1_700_000_000);
}

#[test]
fn remote_update_after_local_delete_restores_without_duplicates() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    fs::write(f.remote_file("file.txt"), "new remote edit").unwrap();
    set_time(&f.remote_file("file.txt"), 1_700_000_100);
    f.pass();
    assert_eq!(conflicts(&f), 1);
    assert_eq!(rows(&f).len(), 2);
    assert_eq!(f.read_local("file.txt"), "new remote edit");
}

#[test]
fn history_for_items_missing_on_both_sides_is_retired_after_complete_pass() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    fs::remove_file(f.remote_file("file.txt")).unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(rows(&f).len(), 1);
    // Recreating the same pathname and timestamp must not delete the new file.
    f.seed("file.txt");
    assert_eq!(f.read_remote("file.txt"), "original");
}

#[test]
fn failed_listing_preserves_history() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    let mut broken = f.remote.clone();
    broken.name = "nonexistent-test-remote".into();
    let pair = pass::Pair {
        db: &f.db,
        remote: &broken,
        dir: &f.dir,
        caps: &f.caps,
    };
    let mut hooks = TestHooks {
        calls: 0,
        on_checkpoint: None,
    };
    assert!(pass::run(&pair, &mut hooks, None).is_err());
    assert_eq!(rows(&f).len(), 1);
    assert_eq!(f.read_remote("file.txt"), "original");
}

#[test]
fn failed_stop_command_preserves_pair_and_history() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    sql(&f, "PRAGMA query_only = ON");
    let mut queue = VecDeque::from([Command::StopDir {
        remote: f.remote.name.clone(),
        local: f.dir.local_path.clone(),
        remote_path: f.dir.remote_path.clone(),
    }]);
    apply_deletes(&mut f.model, &f.db, &mut queue, &f.shared);
    assert!(f.dir.exists(&f.db).unwrap());
    assert_eq!(rows(&f).len(), 1);
    assert!(
        f.model.remotes[&f.remote.name]
            .contains_key(&(f.dir.local_path.clone(), f.dir.remote_path.clone()))
    );
}

#[test]
fn emptied_local_root_preserves_remote_files() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    f.pass();
    assert_eq!(f.read_remote("file.txt"), "original");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn emptied_remote_root_preserves_local_files() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.remote_file("file.txt")).unwrap();
    f.pass();
    assert_eq!(f.read_local("file.txt"), "original");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn partial_deletion_still_propagates() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    f.pass();
    f.assert_no_errors();
    assert!(!f.remote_file("file.txt").exists());
}

#[test]
fn remote_exclusion_file_never_replaces_local_rules() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "*.tmp\n").unwrap();
    set_time(&f.local.join(FILE_IGNORE_NAME), 1_700_000_000);
    let remote_rules = f.remote_file(FILE_IGNORE_NAME);
    fs::write(&remote_rules, "*.other\n").unwrap();
    set_time(&remote_rules, 1_700_000_100);
    f.seed("file.txt");
    assert_eq!(f.read_local(FILE_IGNORE_NAME), "*.tmp\n");
    assert_eq!(fs::read_to_string(&remote_rules).unwrap(), "*.other\n");
    f.assert_clean();
}

#[test]
fn restored_older_remote_version_is_downloaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "restored").unwrap();
    set_time(&f.remote_file("file.txt"), 1_600_000_000);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("file.txt"), "restored");
    f.assert_clean();
}

#[test]
fn restored_older_local_version_is_uploaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "restored").unwrap();
    set_time(&f.local.join("file.txt"), 1_600_000_000);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "restored");
    f.assert_clean();
}

#[test]
fn different_versions_restored_on_both_sides_keep_both() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local restore").unwrap();
    set_time(&f.local.join("file.txt"), 1_600_000_000);
    fs::write(f.remote_file("file.txt"), "remote restore").unwrap();
    set_time(&f.remote_file("file.txt"), 1_600_000_100);
    f.pass();
    // The older (local) version moved to a conflict copy; both versions are
    // on both sides and nothing is left to do.
    let copy = only_conflict_copy(&f, "file");
    assert!(copy.contains("-local-"));
    assert_eq!(f.read_local("file.txt"), "remote restore");
    assert_eq!(f.read_remote("file.txt"), "remote restore");
    assert_eq!(f.read_local(&copy), "local restore");
    assert_eq!(f.read_remote(&copy), "local restore");
    assert_eq!(conflicts(&f), 1);
    f.pass();
    assert_eq!(f.last.as_ref().unwrap().actions, 0);
}

/// The single conflict copy whose name starts with `stem`.
fn only_conflict_copy(f: &Fixture, stem: &str) -> String {
    let copies: Vec<String> = fs::read_dir(&f.local)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(stem) && name.contains(".sync-conflict-"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    copies[0].clone()
}

fn conflicts(f: &Fixture) -> usize {
    store::open_conflicts(&f.db, f.dir.id).unwrap().len()
}

#[test]
fn timestamps_after_2038_and_before_1970_are_recorded() {
    for stamp in [4_000_000_000i64, -86_400] {
        let mut f = Fixture::new();
        fs::write(f.local.join("file.txt"), "time travel").unwrap();
        let time = if stamp >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_secs(stamp.unsigned_abs())
        } else {
            SystemTime::UNIX_EPOCH - Duration::from_secs(stamp.unsigned_abs())
        };
        File::open(f.local.join("file.txt"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
        f.pass();
        f.assert_no_errors();
        assert_eq!(rows(&f)[0].last_local_timestamp, stamp);
        f.assert_clean();
    }
}

#[test]
fn same_size_edit_with_preserved_mtime_is_detected() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    // Same length, same modification time: only the inode change time and
    // the contents differ, as after `cp -p` or `touch -d`.
    fs::write(f.local.join("file.txt"), "ORIGINAL").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "ORIGINAL");
    f.assert_clean();
}

#[test]
fn same_size_remote_edit_with_preserved_mtime_is_detected() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "ORIGINAL").unwrap();
    set_time(&f.remote_file("file.txt"), 1_700_000_000);
    f.pass();
    f.assert_no_errors();
    // The forced transfer is not skipped although size and time match.
    assert_eq!(f.read_local("file.txt"), "ORIGINAL");
    f.assert_clean();
}

#[test]
fn metadata_only_changes_do_not_transfer() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    set_time(&f.local.join("file.txt"), 1_700_000_500);
    f.pass();
    f.assert_no_errors();
    // The remote copy keeps its own time: nothing was uploaded.
    let remote_time = fs::metadata(f.remote_file("file.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        remote_time,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    );
    f.assert_clean();
}

#[test]
fn identical_concurrent_edits_converge_without_conflict() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "same new text").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_100);
    fs::write(f.remote_file("file.txt"), "same new text").unwrap();
    set_time(&f.remote_file("file.txt"), 1_700_000_900);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("file.txt"), "same new text");
    assert_eq!(f.read_remote("file.txt"), "same new text");
    f.assert_clean();
}

#[test]
fn first_sync_of_equal_contents_records_without_copying() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "shared").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    fs::write(f.remote_file("file.txt"), "shared").unwrap();
    set_time(&f.remote_file("file.txt"), 1_500_000_000);
    f.pass();
    f.assert_no_errors();
    let remote_time = fs::metadata(f.remote_file("file.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        remote_time,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000)
    );
    f.assert_clean();
}

#[test]
fn first_sync_with_different_contents_never_overwrites_by_timestamp() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "local newer").unwrap();
    set_time(&f.local.join("file.txt"), 1_900_000_000);
    fs::write(f.remote_file("file.txt"), "remote older").unwrap();
    set_time(&f.remote_file("file.txt"), 1_500_000_000);
    f.pass();
    let copy = only_conflict_copy(&f, "file");
    assert!(copy.contains("-remote-"));
    assert_eq!(f.read_local("file.txt"), "local newer");
    assert_eq!(f.read_remote("file.txt"), "local newer");
    assert_eq!(f.read_local(&copy), "remote older");
    assert_eq!(f.read_remote(&copy), "remote older");
}

#[test]
fn equal_modification_times_move_the_remote_version() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "local text").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    fs::write(f.remote_file("file.txt"), "remote text").unwrap();
    set_time(&f.remote_file("file.txt"), 1_700_000_000);
    f.pass();
    let copy = only_conflict_copy(&f, "file");
    assert!(copy.contains("-remote-"));
    assert_eq!(f.read_local(&copy), "remote text");
    assert_eq!(f.read_remote("file.txt"), "local text");
}

#[test]
fn legacy_history_with_a_missing_side_is_not_a_deletion() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "local").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    fs::write(f.local.join("other.txt"), "other").unwrap();
    fs::write(f.remote_file("other.txt"), "other").unwrap();
    for name in ["file.txt", "other.txt"] {
        sql(
            &f,
            &format!(
                "INSERT INTO sync_items (sync_dir_id, local_path, remote_path, last_local_timestamp, last_remote_timestamp) VALUES ({}, '{}', 'sandbox/{name}', 1700000000, 1700000000)",
                f.dir.id,
                f.local.join(name).display()
            ),
        );
    }
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("file.txt"), "local");
    assert_eq!(f.read_remote("file.txt"), "local");
    assert!(
        store::load_baselines(&f.db, &f.dir)
            .unwrap()
            .values()
            .all(store::Baseline::verified)
    );
}

#[test]
fn one_failing_item_does_not_stop_the_rest_of_the_pair() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    // A name the remote cannot represent is blocked by itself.
    let bad = std::ffi::OsStr::from_bytes(b"bad\xffname");
    fs::write(f.local.join(bad), "x").unwrap();
    fs::write(f.local.join("new.txt"), "new").unwrap();
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    f.pass();
    assert_eq!(f.read_remote("new.txt"), "new");
    assert_eq!(f.read_local("file.txt"), "remote edit");
    assert_eq!(f.model.error_dir_count(), 1);
    assert_eq!(f.errors().len(), 1);
    // A later pass still synchronizes other items while the issue remains.
    fs::write(f.local.join("later.txt"), "later").unwrap();
    f.pass();
    assert_eq!(f.read_remote("later.txt"), "later");
}

#[test]
fn transient_failures_back_off_and_retry_automatically() {
    if !permissions_apply() {
        return;
    }
    let mut f = Fixture::new();
    fs::create_dir(f.remote_root.join("sandbox/locked")).unwrap();
    fs::create_dir(f.local.join("locked")).unwrap();
    f.seed("file.txt");
    fs::write(f.local.join("locked/new.txt"), "new").unwrap();
    let locked = f.remote_root.join("sandbox/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
    f.pass();
    let state = item_states(&f)["locked/new.txt"].clone();
    assert_eq!(state.state, store::RETRYING);
    assert!(state.next_attempt.unwrap() >= store::now() + 50);
    // Not retried before it is due.
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    f.pass();
    assert!(!f.remote_file("locked/new.txt").exists());
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("locked/new.txt"), "new");
}

#[test]
fn leftover_partial_downloads_are_not_uploaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt.Ab3dE6gH.partial"), "half").unwrap();
    f.pass();
    assert!(!f.remote_file("file.txt.Ab3dE6gH.partial").exists());
    assert_eq!(f.errors().len(), 1);
}

#[test]
fn dismissed_limitation_warnings_do_not_reappear() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    // A symbolic link is a limitation: it is shown as a warning, not an error.
    std::os::unix::fs::symlink(
        f.local.join("file.txt"),
        f.local.join("link"),
    )
    .unwrap();
    f.pass();
    assert_eq!(f.errors().len(), 1);
    let error = &f.errors()[0].error;
    assert!(matches!(
        error,
        SyncError::Item {
            state, reason, ..
        } if *state == store::BLOCKED && observe::is_limitation(reason)
    ));
    // The snapshot marks it as a warning.
    let snapshot = f.model.snapshot();
    let warnings = &snapshot["remotes"][0]["dirs"][0]["errors"];
    assert_eq!(warnings[0]["warning"], true);
    // Dismissing it remembers the item; the next pass does not report it again.
    let id = f.errors()[0].id;
    let (remote, local, remote_path) = f.names();
    let mut schedule = Schedule::default();
    dismiss_error(
        &f.db,
        &mut f.model,
        &mut schedule,
        &remote,
        &local,
        &remote_path,
        id,
    );
    assert!(f.errors().is_empty());
    let state = item_states(&f)["link"].clone();
    assert!(state.dismissed);
    // A later pass re-blocks the link but keeps the dismissal.
    f.pass();
    assert!(f.errors().is_empty());
    assert!(item_states(&f)["link"].dismissed);
}

#[test]
fn file_and_folder_with_one_name_are_both_kept() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("thing")).unwrap();
    fs::write(f.local.join("thing/inside.txt"), "inside").unwrap();
    fs::write(f.remote_file("thing"), "a file").unwrap();
    f.pass();
    f.pass();
    // The file moved aside; the folder and the file both exist everywhere.
    let copy = only_conflict_copy(&f, "thing");
    assert_eq!(f.read_local("thing/inside.txt"), "inside");
    assert_eq!(f.read_remote("thing/inside.txt"), "inside");
    assert_eq!(f.read_local(&copy), "a file");
    assert_eq!(f.read_remote(&copy), "a file");
    assert_eq!(conflicts(&f), 1);
}

#[test]
fn unchanged_file_replaced_by_folder_is_propagated() {
    let mut f = Fixture::new();
    f.seed("thing");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("thing")).unwrap();
    fs::create_dir(f.local.join("thing")).unwrap();
    fs::write(f.local.join("thing/inside.txt"), "inside").unwrap();
    set_time(&f.local.join("thing/inside.txt"), 1_700_000_000);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("thing/inside.txt"), "inside");
    f.assert_clean();
}

#[test]
fn unchanged_folder_replaced_by_file_is_propagated() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("thing")).unwrap();
    f.seed("thing/inside.txt");
    f.seed("neighbor.txt");
    fs::remove_dir_all(f.local.join("thing")).unwrap();
    fs::write(f.local.join("thing"), "now a file").unwrap();
    set_time(&f.local.join("thing"), 1_700_000_000);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("thing"), "now a file");
    f.assert_clean();
}

#[test]
fn case_collisions_on_case_insensitive_remotes_are_blocked() {
    let mut f = Fixture::new();
    f.caps.case_insensitive = true;
    fs::write(f.local.join("Readme.md"), "one").unwrap();
    fs::write(f.local.join("README.md"), "two").unwrap();
    f.pass();
    assert!(!f.remote_file("Readme.md").exists());
    assert!(!f.remote_file("README.md").exists());
    assert_eq!(f.errors().len(), 2);
}

#[test]
fn dismissing_a_retrying_item_makes_it_due() {
    let f = Fixture::new();
    store::set_retrying(&f.db, f.dir.id, "x", "offline").unwrap();
    store::retry_now(&f.db, f.dir.id, "x").unwrap();
    assert!(item_states(&f)["x"].next_attempt.unwrap() <= store::now());
}

#[test]
fn remote_checks_are_due_once_per_interval() {
    let mut checks = RemoteChecks::default();
    assert!(checks.due(1));
    checks.done(1);
    assert!(!checks.due(1));
    assert!(checks.due(2));
    checks.forget(1);
    assert!(checks.due(1));
}

#[test]
fn capabilities_are_recorded_per_remote() {
    let f = Fixture::new();
    assert_eq!(f.caps.preferred_hash(), Some("md5"));
    assert_eq!(
        store::load_capabilities(&f.db, &f.remote.name).unwrap(),
        Some(f.caps.clone())
    );
    assert_eq!(store::load_capabilities(&f.db, "other").unwrap(), None);
}

#[test]
fn database_is_backed_up_before_a_schema_change() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("celestial.db");
    File::create(&path).unwrap();
    let db = util::await_future(Database::connect(format!("sqlite://{}", path.display()))).unwrap();
    util::await_future(Migrator::up(&db, Some(4))).unwrap();
    backup_before_migration(&db, &path).unwrap();
    let backups = fs::read_dir(temp.path())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("before-upgrade")
        })
        .count();
    assert_eq!(backups, 1);
    util::await_future(Migrator::up(&db, None)).unwrap();
    // Nothing is pending afterwards, so no further copies are made.
    backup_before_migration(&db, &path).unwrap();
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn malformed_remote_names_are_errors() {
    let _guard = crate::sync::testing::rclone_guard();
    assert!(rclone::sync::stat("bad:", "x").is_err());
    assert!(rclone::sync::list("", "x", false, RcloneListFilter::All).is_err());
}

#[test]
fn racy_files_are_rechecked_before_their_tuple_is_trusted() {
    let mut f = Fixture::new();
    crate::sync::observe::set_racy_window(2_000_000_000);
    fs::write(f.local.join("file.txt"), "just written").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    f.pass();
    f.assert_no_errors();
    // Written within the timestamp granularity: contents are compared again
    // on the next pass instead of trusting the metadata.
    let baseline = &store::load_baselines(&f.db, &f.dir).unwrap()["file.txt"];
    assert!(baseline.verified());
    assert!(baseline.local.is_none());
    assert!(baseline.fingerprint.is_some());
}

fn recovered(f: &Fixture) -> Vec<store::RecoveryItem> {
    let pair = f.pair();
    pass::recovery_list(&pair).unwrap()
}

#[test]
fn propagated_deletions_are_recoverable_on_both_sides() {
    for delete_local in [true, false] {
        let mut f = Fixture::new();
        f.seed("file.txt");
        f.seed("neighbor.txt");
        if delete_local {
            fs::remove_file(f.local.join("file.txt")).unwrap();
        } else {
            fs::remove_file(f.remote_file("file.txt")).unwrap();
        }
        f.pass();
        f.assert_no_errors();
        assert!(!f.local.join("file.txt").exists());
        assert!(!f.remote_file("file.txt").exists());
        let items = recovered(&f);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].side, if delete_local { "remote" } else { "local" });
        // Restoring puts it back on that side and the next pass propagates it.
        let pair = f.pair();
        pass::restore(&pair, items[0].id).unwrap();
        f.pass();
        f.assert_no_errors();
        assert_eq!(f.read_local("file.txt"), "original");
        assert_eq!(f.read_remote("file.txt"), "original");
    }
}

#[test]
fn replaced_versions_are_preserved_in_both_directions() {
    let mut f = Fixture::new();
    f.seed("down.txt");
    f.seed("up.txt");
    fs::write(f.remote_file("down.txt"), "remote edit").unwrap();
    fs::write(f.local.join("up.txt"), "local edit").unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("down.txt"), "remote edit");
    assert_eq!(f.read_remote("up.txt"), "local edit");
    let items = recovered(&f);
    assert_eq!(items.len(), 2);
    for item in &items {
        let text = if item.side == "local" {
            fs::read_to_string(&item.stored_path).unwrap()
        } else {
            fs::read_to_string(f.remote_root.join(&item.stored_path)).unwrap()
        };
        assert_eq!(text, "original", "{item:?}");
    }
    // Restoring a version preserves the current one first.
    let local_item = items.iter().find(|item| item.side == "local").unwrap();
    let pair = f.pair();
    pass::restore(&pair, local_item.id).unwrap();
    assert_eq!(f.read_local("down.txt"), "original");
    assert_eq!(recovered(&f).len(), 3);
}

#[test]
fn internal_folder_is_never_synchronized() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let internal = fs::read_dir(&f.local)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .find(|name| name.starts_with(".celestial-"))
        .expect("internal folder");
    assert!(!f.remote_file(&internal).exists());
    // A remote item with that name is reported, never downloaded into it.
    fs::create_dir(f.remote_file(&internal)).unwrap();
    fs::write(f.remote_file(&format!("{internal}/x")), "x").unwrap();
    f.pass();
    assert!(!f.local.join(&internal).join("x").exists());
    assert_eq!(f.errors().len(), 1);
}

#[test]
fn preexisting_folders_are_never_used_as_the_internal_folder() {
    let mut f = Fixture::new();
    let uuid = store::pair_uuid(&f.db, f.dir.id).unwrap();
    let taken = f.local.join(format!(".celestial-{}", &uuid[..8]));
    fs::create_dir(&taken).unwrap();
    fs::write(taken.join("mine.txt"), "user data").unwrap();
    f.pass();
    f.assert_no_errors();
    // The person's folder is synchronized like any other.
    assert_eq!(
        fs::read_to_string(
            f.remote_root
                .join("sandbox")
                .join(taken.file_name().unwrap())
                .join("mine.txt")
        )
        .unwrap(),
        "user data"
    );
}

#[test]
fn deletion_after_an_edit_delete_outcome_is_bound_to_what_was_shown() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    f.pass();
    assert_eq!(f.read_local("file.txt"), "remote edit");
    let record = store::open_conflicts(&f.db, f.dir.id).unwrap().remove(0);
    assert_eq!(record.kind, "edit_delete");
    let pair = f.pair();
    let token = pass::item_token(&pair, "file.txt").unwrap();
    // A change after the prompt was shown refuses the deletion.
    fs::write(f.remote_file("file.txt"), "newer remote edit").unwrap();
    assert!(pass::delete_everywhere(&pair, "file.txt", &token).is_err());
    assert_eq!(f.read_remote("file.txt"), "newer remote edit");
    let token = pass::item_token(&pair, "file.txt").unwrap();
    pass::delete_everywhere(&pair, "file.txt", &token).unwrap();
    assert!(!f.local.join("file.txt").exists());
    assert!(!f.remote_file("file.txt").exists());
    assert_eq!(recovered(&f).len(), 2);
    f.pass();
    f.assert_no_errors_except_conflicts();
}

#[test]
fn acknowledging_a_conflict_removes_it() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "local").unwrap();
    fs::write(f.remote_file("file.txt"), "remote").unwrap();
    f.pass();
    let id = f.errors()[0].id;
    let (remote, local, remote_path) = f.names();
    let mut schedule = Schedule::default();
    dismiss_error(
        &f.db,
        &mut f.model,
        &mut schedule,
        &remote,
        &local,
        &remote_path,
        id,
    );
    assert_eq!(conflicts(&f), 0);
    f.pass();
    f.assert_no_errors();
}

#[test]
fn retention_keeps_recent_versions_for_the_chosen_number_of_days() {
    let mut f = Fixture::new();
    f.seed("live.txt");
    let internal = recovery::ensure_internal(&f.db, &f.dir).unwrap();
    let uuid = internal.pair_uuid.clone();
    let add = |rel: &str, age_days: i64, index: usize| {
        let path = internal.recovery().join(format!("{rel}.{index}"));
        fs::write(&path, "old").unwrap();
        store::insert_recovery(
            &f.db,
            &store::RecoveryItem {
                id: 0,
                pair_uuid: uuid.clone(),
                remote_name: f.remote.name.clone(),
                local_root: f.dir.local_path.clone(),
                remote_root: f.dir.remote_path.clone(),
                rel: rel.into(),
                side: "local".into(),
                stored_path: path.display().to_string(),
                reason: "test".into(),
                op_id: format!("op{rel}{index}"),
                size: Some(3),
                mtime: Some(0),
                created_at: store::now() - age_days * 86_400 + index as i64,
            },
        )
        .unwrap();
        path
    };
    let recent: Vec<_> = (0..12).map(|index| add("live.txt", 0, index)).collect();
    let expired = add("gone.txt", 40, 0);
    let week_old = add("gone2.txt", 7, 0);
    let root = crate::sync::localfs::Root::open(&f.local, None).unwrap();
    let rec = recovery::Store {
        db: &f.db,
        remote: &f.remote,
        dir: &f.dir,
        internal: &internal,
        root: &root,
    };
    let live = ["live.txt".to_owned()].into();
    rec.prune(&live).unwrap();
    // Ten newest versions of a live item remain.
    assert_eq!(recent.iter().filter(|path| path.exists()).count(), 10);
    assert!(!recent[0].exists() && !recent[1].exists());
    // The only copy of a deleted item is kept while it is recent enough.
    assert!(!expired.exists());
    assert!(week_old.exists());
    // Kept for good: nothing more goes, however old.
    store::set_keep_versions_days(&f.db, f.dir.id, 0).unwrap();
    let ancient = add("gone3.txt", 4000, 0);
    rec.prune(&live).unwrap();
    assert!(ancient.exists() && week_old.exists());
    // A shorter period applies on the next prune.
    store::set_keep_versions_days(&f.db, f.dir.id, 1).unwrap();
    rec.prune(&live).unwrap();
    assert!(!ancient.exists() && !week_old.exists());
    assert_eq!(recent.iter().filter(|path| path.exists()).count(), 10);
}

#[test]
fn conflict_copies_are_capped_per_item() {
    let mut f = Fixture::new();
    for index in 0..12u64 {
        let name = recovery::conflict_rel(
            "file.txt",
            "local",
            &format!("{index:08x}"),
            crate::sync::observe::Stamp::new(1_700_000_000 + index as i64 * 60, 0),
        );
        fs::write(f.local.join(&name), format!("copy {index}")).unwrap();
        set_time(&f.local.join(&name), 1_700_000_000);
    }
    f.pass();
    f.pass();
    f.assert_no_errors();
    let visible = |root: &Path| {
        fs::read_dir(root)
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".sync-conflict-")
            })
            .count()
    };
    assert_eq!(visible(&f.local), recovery::MAX_CONFLICT_COPIES);
    assert_eq!(
        visible(&f.remote_root.join("sandbox")),
        recovery::MAX_CONFLICT_COPIES
    );
    // The two oldest went to recovery on both sides.
    assert_eq!(recovered(&f).len(), 4);
}

/// Run a pass that is interrupted at `point`, as by a crash or a kill.
fn crash_at(f: &mut Fixture, point: &'static str) {
    crate::sync::failpoint::arm(point);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f.pass()));
    crate::sync::failpoint::disarm();
    assert!(result.is_err(), "{point} was not reached");
}

fn staging_entries(f: &Fixture) -> usize {
    let internal = recovery::ensure_internal(&f.db, &f.dir).unwrap();
    fs::read_dir(internal.staging()).unwrap().count()
}

#[test]
fn interrupted_local_preservation_is_recorded_on_restart() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.remote_file("file.txt")).unwrap();
    crash_at(&mut f, "preserve_local:after_move");
    assert!(!f.local.join("file.txt").exists());
    assert!(recovered(&f).is_empty());
    f.pass();
    f.assert_no_errors();
    let items = recovered(&f);
    assert_eq!(items.len(), 1);
    assert_eq!(
        fs::read_to_string(&items[0].stored_path).unwrap(),
        "original"
    );
    assert!(!f.remote_file("file.txt").exists());
}

#[test]
fn interrupted_remote_preservation_is_recorded_on_restart() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    crash_at(&mut f, "preserve_remote:after_move");
    f.pass();
    f.assert_no_errors();
    let items = recovered(&f);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].side, "remote");
    assert_eq!(
        fs::read_to_string(f.remote_root.join(&items[0].stored_path)).unwrap(),
        "original"
    );
}

#[test]
fn download_interrupted_before_commit_leaves_the_local_file_and_no_stage() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    crash_at(&mut f, "download:staged");
    assert_eq!(f.read_local("file.txt"), "original");
    assert_eq!(staging_entries(&f), 1);
    f.pass();
    f.assert_no_errors();
    assert_eq!(staging_entries(&f), 0);
    assert_eq!(f.read_local("file.txt"), "remote edit");
}

#[test]
fn download_interrupted_after_commit_preserves_the_displaced_version() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    crash_at(&mut f, "download:exchanged");
    assert_eq!(f.read_local("file.txt"), "remote edit");
    f.pass();
    f.assert_no_errors();
    assert_eq!(staging_entries(&f), 0);
    let items = recovered(&f);
    assert_eq!(items.len(), 1);
    assert_eq!(
        fs::read_to_string(&items[0].stored_path).unwrap(),
        "original"
    );
    f.assert_clean();
}

#[test]
fn upload_interrupted_after_preserving_puts_the_remote_version_back() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    crash_at(&mut f, "upload:before_transfer");
    assert!(!f.remote_file("file.txt").exists());
    // The replay restores the previous version before anything is decided.
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "local edit");
    let items = recovered(&f);
    assert!(items.iter().all(|item| item.side == "remote"));
    f.assert_clean();
}

#[test]
fn keep_both_interrupted_after_the_rename_still_records_the_conflict() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "local").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    fs::write(f.remote_file("file.txt"), "remote").unwrap();
    set_time(&f.remote_file("file.txt"), 1_700_000_100);
    crash_at(&mut f, "keep_both:renamed");
    f.pass();
    f.pass();
    assert_eq!(conflicts(&f), 1);
    let copy = only_conflict_copy(&f, "file");
    assert_eq!(f.read_local(&copy), "local");
    assert_eq!(f.read_remote(&copy), "local");
    assert_eq!(f.read_local("file.txt"), "remote");
    assert_eq!(f.read_remote("file.txt"), "remote");
}

#[test]
fn stray_partial_files_in_staging_are_removed_not_uploaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let internal = recovery::ensure_internal(&f.db, &f.dir).unwrap();
    fs::write(internal.staging().join("abc.Ab3dE6gH.partial"), "half").unwrap();
    f.pass();
    assert_eq!(staging_entries(&f), 0);
    f.assert_clean();
}

#[test]
fn a_folder_swapped_for_a_link_after_observation_is_not_followed() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("dir")).unwrap();
    f.seed("dir/file.txt");
    fs::write(f.remote_file("dir/file.txt"), "remote edit").unwrap();
    let outside = f.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file.txt"), "outside").unwrap();
    let local = f.local.clone();
    let mut swapped = false;
    f.pass_with(|_| {
        if !swapped {
            swapped = true;
            fs::rename(local.join("dir"), local.join("moved")).unwrap();
            std::os::unix::fs::symlink(&outside, local.join("dir")).unwrap();
        }
    });
    assert_eq!(
        fs::read_to_string(outside.join("file.txt")).unwrap(),
        "outside"
    );
    assert_eq!(f.read_local("moved/file.txt"), "original");
}

#[test]
fn uploads_to_remotes_without_hashes_use_a_snapshot() {
    let mut f = Fixture::new();
    f.caps.hashes.clear();
    fs::write(f.local.join("file.txt"), "content").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "content");
    // The snapshot carries the original modification time.
    assert_eq!(
        fs::metadata(f.remote_file("file.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    );
    assert_eq!(staging_entries(&f), 0);
    let baseline = &store::load_baselines(&f.db, &f.dir).unwrap()["file.txt"];
    assert_eq!(baseline.fingerprint.as_ref().unwrap().algo, "md5");
    f.assert_clean();
}

#[test]
fn remote_edit_inside_the_upload_window_becomes_a_conflict_copy() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    let remote_file = f.remote_file("file.txt");
    // Another client edits the remote after the check, before the previous
    // version is moved aside: the move preserves the edit, and it stays
    // visible as a conflict copy.
    crate::sync::failpoint::arm_action("preserve_remote:before_move", move || {
        fs::write(&remote_file, "remote edit!").unwrap();
    });
    f.pass();
    crate::sync::failpoint::disarm();
    f.pass();
    assert_eq!(f.read_remote("file.txt"), "local edit");
    let copy = only_conflict_copy(&f, "file");
    assert_eq!(f.read_remote(&copy), "remote edit!");
    assert_eq!(f.read_local(&copy), "remote edit!");
    assert_eq!(conflicts(&f), 1);
}

#[test]
fn local_edit_inside_the_download_window_becomes_a_conflict_copy() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    let local_file = f.local.join("file.txt");
    crate::sync::failpoint::arm_action("download:before_exchange", move || {
        fs::write(&local_file, "local edit!!").unwrap();
    });
    f.pass();
    crate::sync::failpoint::disarm();
    f.pass();
    assert_eq!(f.read_local("file.txt"), "remote edit");
    let copy = only_conflict_copy(&f, "file");
    assert_eq!(f.read_local(&copy), "local edit!!");
    assert_eq!(f.read_remote(&copy), "local edit!!");
}

#[test]
fn slow_passes_back_off_the_remote_schedule() {
    let mut checks = RemoteChecks::default();
    checks.done_after(1, Duration::from_secs(5));
    assert!(!checks.due(1));
    checks.done_after(2, Duration::from_secs(90));
    assert_eq!(checks.0[&2].1, Duration::from_secs(180));
    assert_eq!(checks.0[&1].1, REMOTE_CHECK_INTERVAL);
}

#[test]
fn quiet_pairs_reuse_their_observation_until_an_event_or_the_remote_check() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let mut watcher = watch::LocalWatcher::new();
    watcher.sync_pairs(&[(f.dir.id, f.local.clone())]);
    let mut schedule = Schedule::default();
    let (rx_tx, rx) = crossbeam_channel::unbounded();
    drop(rx_tx);
    let mut queue = VecDeque::new();
    let caps = f.caps.clone();
    let remote = f.remote.clone();
    let dir = f.dir.clone();
    let mut run = |f: &mut Fixture, schedule: &mut Schedule, watcher: &mut watch::LocalWatcher| {
        sync_pair(
            &f.shared,
            &f.db,
            &mut f.model,
            &rx,
            &mut queue,
            &remote,
            &dir,
            &caps,
            watcher,
            schedule,
        );
    };
    run(&mut f, &mut schedule, &mut watcher);
    assert!(schedule.local_trees.contains_key(&f.dir.id));
    // A remote edit is found on the remote schedule without local activity.
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    schedule.remote_checks.forget(f.dir.id);
    run(&mut f, &mut schedule, &mut watcher);
    assert_eq!(f.read_local("file.txt"), "remote edit");
    // A local edit is seen through the watcher.
    thread::sleep(Duration::from_millis(300));
    run(&mut f, &mut schedule, &mut watcher);
    fs::write(f.local.join("new.txt"), "new").unwrap();
    let mut uploaded = false;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(100));
        run(&mut f, &mut schedule, &mut watcher);
        if f.remote_file("new.txt").exists() {
            uploaded = true;
            break;
        }
    }
    assert!(uploaded);
}

#[test]
fn atomic_save_storms_converge_to_the_last_version() {
    let mut f = Fixture::new();
    f.seed("doc.txt");
    for round in 0..50 {
        let temp = f.local.join(format!(".doc.txt.tmp{round}"));
        fs::write(&temp, format!("version {round}")).unwrap();
        fs::rename(&temp, f.local.join("doc.txt")).unwrap();
    }
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("doc.txt"), "version 49");
    assert_eq!(conflicts(&f), 0);
    assert!(
        !fs::read_dir(f.remote_root.join("sandbox"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp"))
    );
}

#[test]
fn an_offline_remote_pauses_the_pair_and_recovers_without_action() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let sandbox = f.remote_root.join("sandbox");
    let away = f.remote_root.join("away");
    fs::rename(&sandbox, &away).unwrap();
    fs::write(f.local.join("file.txt"), "edited offline").unwrap();
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert_eq!(f.read_local("file.txt"), "edited offline");
    fs::rename(&away, &sandbox).unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("file.txt"), "edited offline");
}

#[test]
fn an_idle_pass_reads_no_contents_and_lists_once() {
    let mut f = Fixture::new();
    for index in 0..50 {
        fs::write(f.local.join(format!("file{index}.txt")), "data").unwrap();
        set_time(&f.local.join(format!("file{index}.txt")), 1_700_000_000);
    }
    f.pass();
    f.assert_no_errors();
    f.pass();
    let stats = f.last.as_ref().unwrap().stats;
    assert_eq!(f.last.as_ref().unwrap().actions, 0);
    assert_eq!(stats.hashed_bytes, 0, "{stats:?}");
    // One recursive listing; everything else is metadata already known.
    assert!(stats.rpc_calls <= 2, "{stats:?}");
    assert_eq!(stats.local_items, 50);
}

/// The audit's 20,000-file fixture. Run explicitly; results are recorded in
/// docs/synchronization-validation.md.
#[test]
#[ignore]
fn measure_twenty_thousand_files() {
    let mut f = Fixture::new();
    for dir in 0..100 {
        fs::create_dir(f.local.join(format!("d{dir}"))).unwrap();
        for file in 0..200 {
            let path = f.local.join(format!("d{dir}/f{file}.txt"));
            fs::write(&path, format!("{dir}/{file}")).unwrap();
            set_time(&path, 1_700_000_000);
        }
    }
    let started = std::time::Instant::now();
    f.pass();
    let first = started.elapsed();
    f.assert_no_errors();
    let initial = f.last.as_ref().unwrap().stats;
    let started = std::time::Instant::now();
    f.pass();
    let idle = started.elapsed();
    let idle_stats = f.last.as_ref().unwrap().stats;
    println!("initial upload: {first:?} {initial:?}");
    println!("idle pass: {idle:?} {idle_stats:?}");
    assert_eq!(idle_stats.hashed_bytes, 0);
    assert_eq!(f.last.as_ref().unwrap().actions, 0);
}

#[test]
fn case_insensitive_backends_are_detected_and_collisions_blocked() {
    use crate::sync::testing::Backend;
    let mut f = Fixture::with_backend(Backend::CaseInsensitive);
    assert!(f.caps.case_insensitive);
    fs::write(f.local.join("Notes.txt"), "one").unwrap();
    fs::write(f.local.join("notes.txt"), "two").unwrap();
    fs::write(f.local.join("other.txt"), "other").unwrap();
    f.pass();
    assert_eq!(f.read_remote("other.txt"), "other");
    assert_eq!(f.errors().len(), 2);
    assert_eq!(f.read_local("Notes.txt"), "one");
    assert_eq!(f.read_local("notes.txt"), "two");
}

#[test]
fn backends_without_server_side_move_still_preserve_versions() {
    use crate::sync::testing::Backend;
    let mut f = Fixture::with_backend(Backend::Memory);
    assert!(!f.caps.server_move);
    f.pass();
    f.seed("file.txt");
    f.seed("other.txt");
    assert_eq!(f.cat_remote("file.txt").as_deref(), Some("original"));
    // An overwrite and a deletion both preserve the previous remote version
    // through rclone's copy-and-delete fallback.
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    fs::remove_file(f.local.join("other.txt")).unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.cat_remote("file.txt").as_deref(), Some("local edit"));
    assert_eq!(f.cat_remote("other.txt"), None);
    let items = recovered(&f);
    assert_eq!(items.len(), 2);
    for item in items {
        let target = f.temp.path().join("recovered");
        rclone::sync::copy_to_local(target.to_str().unwrap(), &f.remote.name, &item.stored_path)
            .unwrap();
        assert_eq!(fs::read_to_string(target).unwrap(), "original");
    }
}

#[test]
fn without_hashes_equal_size_and_time_count_as_equal_contents() {
    // Generic WebDAV: no hash, so a same-size edit within the provider's
    // precision cannot be seen without downloading. Different sizes or
    // times are still different contents.
    let mut f = Fixture::new();
    f.caps.hashes.clear();
    f.caps.precision_ns = 1_000_000_000;
    fs::write(f.local.join("same.txt"), "aaaa").unwrap();
    set_time(&f.local.join("same.txt"), 1_700_000_000);
    fs::write(f.remote_file("same.txt"), "bbbb").unwrap();
    set_time(&f.remote_file("same.txt"), 1_700_000_000);
    fs::write(f.local.join("differ.txt"), "local").unwrap();
    set_time(&f.local.join("differ.txt"), 1_700_000_000);
    fs::write(f.remote_file("differ.txt"), "remote!").unwrap();
    set_time(&f.remote_file("differ.txt"), 1_700_000_000);
    f.pass();
    assert_eq!(conflicts(&f), 1);
    assert_eq!(f.read_local("same.txt"), "aaaa");
    assert_eq!(f.read_remote("same.txt"), "bbbb");
}

#[test]
fn an_item_missing_from_a_delayed_listing_is_not_deleted() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("other.txt");
    // The listing misses the file (eventual consistency) but it exists by
    // the time Celestial would act on that observation.
    let hidden = f.temp.path().join("hidden");
    fs::rename(f.remote_file("file.txt"), &hidden).unwrap();
    let target = f.remote_file("file.txt");
    let mut restored = false;
    f.pass_with(|_| {
        if !restored {
            restored = true;
            fs::rename(&hidden, &target).unwrap();
        }
    });
    assert_eq!(f.read_local("file.txt"), "original");
    assert_eq!(f.read_remote("file.txt"), "original");
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("file.txt"), "original");
}

#[test]
fn nested_folders_converge_and_stay_clean() {
    let mut f = Fixture::new();
    for dir in ["a", "a/b", "a/b/c", "x", "x/empty"] {
        fs::create_dir(f.local.join(dir)).unwrap();
    }
    for file in ["a/1", "a/b/2", "a/b/c/3", "x/4"] {
        fs::write(f.local.join(file), file).unwrap();
        set_time(&f.local.join(file), 1_700_000_000);
    }
    fs::create_dir_all(f.remote_root.join("sandbox/r/s")).unwrap();
    fs::write(f.remote_file("r/s/5"), "5").unwrap();
    f.pass();
    f.assert_no_errors();
    assert!(f.remote_file("x/empty").is_dir());
    assert_eq!(f.read_local("r/s/5"), "5");
    f.assert_clean();
    f.assert_clean();
}

#[test]
fn a_corrupt_baseline_never_loses_either_side() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    sql(
        &f,
        "UPDATE sync_items SET hash = 'garbage', remote_hash = 'garbage', local_inode = 1",
    );
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    f.pass();
    // Both sides look changed against the damaged baseline: both versions
    // survive.
    let mut texts = vec![f.read_local("file.txt"), f.read_remote("file.txt")];
    if let Ok(entries) = fs::read_dir(&f.local) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .contains(".sync-conflict-")
            {
                texts.push(fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    assert!(texts.contains(&"original".to_owned()), "{texts:?}");
    assert!(texts.contains(&"remote edit".to_owned()), "{texts:?}");
}

#[test]
fn long_names_get_conflict_copies_that_fit() {
    let mut f = Fixture::new();
    let name = format!("{}.txt", "n".repeat(240));
    fs::write(f.local.join(&name), "local").unwrap();
    set_time(&f.local.join(&name), 1_700_000_000);
    fs::write(f.remote_file(&name), "remote").unwrap();
    set_time(&f.remote_file(&name), 1_700_000_100);
    f.pass();
    let copy = only_conflict_copy(&f, "nnnn");
    assert!(copy.len() <= 255);
    assert!(copy.ends_with(".txt"));
    assert_eq!(f.read_remote(&copy), "local");
}

#[test]
fn unicode_normalization_variants_are_blocked() {
    let mut f = Fixture::new();
    fs::write(f.local.join("caf\u{e9}.txt"), "composed").unwrap();
    fs::write(f.remote_file("cafe\u{301}.txt"), "decomposed").unwrap();
    f.pass();
    assert_eq!(f.errors().len(), 2);
    assert!(!f.local.join("cafe\u{301}.txt").exists());
    assert!(!f.remote_file("caf\u{e9}.txt").exists());
}

#[test]
fn settings_read_failures_are_errors_not_empty_configuration() {
    let f = Fixture::new();
    sql(&f, "DROP TABLE sync_dirs");
    let mut model = Model::new();
    assert!(reload_model(&f.db, &mut model).is_err());
}

#[test]
fn paused_pairs_stay_paused_until_started() {
    let f = Fixture::new();
    let (local, remote_path) = (&f.dir.local_path, &f.dir.remote_path);
    store::set_paused(&f.db, f.dir.id, true).unwrap();
    let mut model = Model::new();
    reload_model(&f.db, &mut model).unwrap();
    let dir = model.dir(&f.remote.name, local, remote_path).unwrap();
    assert!(dir.paused);
    assert_eq!(dir.phase, "paused");

    let mut schedule = Schedule::default();
    let mut queue = VecDeque::from([Command::ResumeDir {
        remote: f.remote.name.clone(),
        local: local.clone(),
        remote_path: remote_path.clone(),
    }]);
    apply_between_passes(
        &f.shared,
        &f.db,
        &mut model,
        &mut queue,
        &mut schedule,
        &mut HashMap::new(),
    );
    assert!(store::paused_pairs(&f.db).unwrap().is_empty());
    let dir = model.dir(&f.remote.name, local, remote_path).unwrap();
    assert!(!dir.paused);
    assert!(schedule.remote_checks.due(f.dir.id));
}

#[test]
fn versions_of_unresolved_conflicts_are_never_pruned() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let internal = recovery::ensure_internal(&f.db, &f.dir).unwrap();
    let old = internal.recovery().join("old");
    fs::write(&old, "old").unwrap();
    let item = store::RecoveryItem {
        id: 0,
        pair_uuid: internal.pair_uuid.clone(),
        remote_name: f.remote.name.clone(),
        local_root: f.dir.local_path.clone(),
        remote_root: f.dir.remote_path.clone(),
        rel: "file.txt".into(),
        side: "local".into(),
        stored_path: old.display().to_string(),
        reason: "test".into(),
        op_id: "conflict-op".into(),
        size: Some(3),
        mtime: Some(0),
        created_at: store::now() - 90 * 86_400,
    };
    store::insert_recovery(&f.db, &item).unwrap();
    store::insert_conflict(
        &f.db,
        f.dir.id,
        &internal.pair_uuid,
        "file.txt",
        "edit_edit",
        "{}",
        None,
        "conflict-op",
    )
    .unwrap();
    f.pass();
    assert!(old.exists());
}

#[test]
fn without_hashes_or_times_equal_sizes_are_compared_by_contents() {
    // Generic WebDAV keeps neither hashes nor modification times, so equal
    // sizes prove nothing: the remote version is downloaded and compared.
    let mut f = Fixture::new();
    f.caps.hashes.clear();
    f.caps.set_modtime = false;
    f.caps.precision_ns = rclone::MOD_TIME_NOT_SUPPORTED;
    fs::write(f.local.join("same.txt"), "aaaa").unwrap();
    set_time(&f.local.join("same.txt"), 1_600_000_000);
    fs::write(f.remote_file("same.txt"), "aaaa").unwrap();
    set_time(&f.remote_file("same.txt"), 1_700_000_000);
    fs::write(f.local.join("differ.txt"), "aaaa").unwrap();
    set_time(&f.local.join("differ.txt"), 1_700_000_000);
    fs::write(f.remote_file("differ.txt"), "bbbb").unwrap();
    set_time(&f.remote_file("differ.txt"), 1_700_000_000);
    f.pass();
    f.assert_no_errors_except_conflicts();
    assert_eq!(conflicts(&f), 1);
    let copy = only_conflict_copy(&f, "differ");
    assert_eq!(f.read_local(&copy), "bbbb");
    assert_eq!(f.read_remote(&copy), "bbbb");
    assert_eq!(f.read_local("differ.txt"), "aaaa");
    assert_eq!(f.read_remote("differ.txt"), "aaaa");
    assert!(fs::read_dir(&f.local).unwrap().flatten().all(|entry| {
        !entry
            .file_name()
            .to_string_lossy()
            .starts_with("same.txt.sync-conflict")
    }));
    assert_eq!(f.read_remote("same.txt"), "aaaa");
    assert_eq!(staging_entries(&f), 0);
    f.pass();
    assert_eq!(f.last.as_ref().unwrap().actions, 0);
}

#[test]
fn an_interrupted_keep_both_is_not_mistaken_for_a_deletion() {
    // Interrupted right after the rename, before or after the history of
    // the renamed version was dropped.
    for point in ["keep_both:moved", "keep_both:renamed"] {
        let mut f = Fixture::new();
        f.seed("file.txt");
        f.seed("other.txt");
        fs::write(f.local.join("file.txt"), "local edit").unwrap();
        set_time(&f.local.join("file.txt"), 1_700_000_100);
        fs::write(f.remote_file("file.txt"), "remote edit!").unwrap();
        set_time(&f.remote_file("file.txt"), 1_700_000_200);
        crash_at(&mut f, point);
        f.pass();
        f.pass();
        f.assert_no_errors_except_conflicts();
        let records = store::open_conflicts(&f.db, f.dir.id).unwrap();
        assert_eq!(records.len(), 1, "{point}: {records:?}");
        assert_eq!(records[0].kind, "edit_edit");
        let copy = only_conflict_copy(&f, "file");
        assert_eq!(f.read_local(&copy), "local edit");
        assert_eq!(f.read_remote(&copy), "local edit");
        assert_eq!(f.read_local("file.txt"), "remote edit!");
        assert_eq!(f.read_remote("file.txt"), "remote edit!");
    }
}

fn internal_folders(f: &Fixture) -> Vec<String> {
    fs::read_dir(&f.local)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with(".celestial-"))
        .collect()
}

#[test]
fn internal_folders_of_stopped_pairs_are_never_uploaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    f.pass();
    f.assert_no_errors();
    let old = internal_folders(&f);
    assert_eq!(old.len(), 1);
    // Stopping the pair and adding the folder again gives it a new identity
    // and no history; the old internal folder still holds a version.
    sql(
        &f,
        "UPDATE sync_dirs SET pair_uuid = 'feedfacefeedfacefeedfacefeedface', internal_name = NULL, internal_dev = NULL, internal_ino = NULL",
    );
    sql(&f, "DELETE FROM sync_items");
    f.pass();
    f.assert_no_errors();
    assert_eq!(internal_folders(&f).len(), 2);
    for name in internal_folders(&f) {
        assert!(!f.remote_file(&name).exists(), "{name} was uploaded");
    }
    f.assert_clean();
}

#[test]
fn a_copied_internal_folder_is_adopted_not_uploaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_file("file.txt"), "remote edit").unwrap();
    f.pass();
    f.assert_no_errors();
    let [name] = internal_folders(&f).try_into().unwrap();
    // Restoring or copying the synced folder gives the internal folder a
    // new inode; its marker still says whose it is.
    let aside = f.temp.path().join("aside");
    fs::rename(f.local.join(&name), &aside).unwrap();
    let copied = std::process::Command::new("cp")
        .arg("-r")
        .arg(&aside)
        .arg(f.local.join(&name))
        .status()
        .unwrap();
    assert!(copied.success());
    f.pass();
    f.assert_no_errors();
    assert_eq!(internal_folders(&f), vec![name.clone()]);
    assert!(!f.remote_file(&name).exists());
    let items = recovered(&f);
    assert_eq!(items.len(), 1);
    assert_eq!(
        fs::read_to_string(&items[0].stored_path).unwrap(),
        "original"
    );
    f.assert_clean();
}

#[test]
fn letter_case_renames_on_case_insensitive_remotes_are_propagated() {
    let mut f = Fixture::new();
    f.caps.case_insensitive = true;
    f.seed("readme.txt");
    fs::rename(f.local.join("readme.txt"), f.local.join("README.txt")).unwrap();
    f.pass();
    f.assert_no_errors();
    assert!(!f.remote_file("readme.txt").exists());
    assert_eq!(f.read_remote("README.txt"), "original");
    f.assert_clean();
    fs::rename(f.remote_file("README.txt"), f.remote_file("Readme.txt")).unwrap();
    f.pass();
    f.assert_no_errors();
    assert!(!f.local.join("README.txt").exists());
    assert_eq!(f.read_local("Readme.txt"), "original");
    f.assert_clean();
}

#[test]
fn a_letter_case_rename_that_met_an_edit_stays_blocked() {
    let mut f = Fixture::new();
    f.caps.case_insensitive = true;
    f.seed("notes.txt");
    fs::rename(f.local.join("notes.txt"), f.local.join("Notes.txt")).unwrap();
    fs::write(f.remote_file("notes.txt"), "remote edit").unwrap();
    f.pass();
    assert_eq!(f.errors().len(), 2);
    assert_eq!(f.read_remote("notes.txt"), "remote edit");
    assert!(!f.remote_file("Notes.txt").exists());
    assert_eq!(f.read_local("Notes.txt"), "original");
    assert!(!f.local.join("notes.txt").exists());
}

#[test]
fn a_failed_remote_restore_puts_the_current_version_back() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("other.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    f.pass();
    f.assert_no_errors();
    let item = recovered(&f)
        .into_iter()
        .find(|item| item.side == "remote")
        .unwrap();
    let stored = f.remote_root.join(&item.stored_path);
    crate::sync::failpoint::arm_action("restore:before_copy", move || {
        fs::remove_file(&stored).unwrap();
    });
    let restored = pass::restore(&f.pair(), item.id);
    crate::sync::failpoint::disarm();
    assert!(restored.is_err());
    assert_eq!(f.read_remote("file.txt"), "local edit");
    let restored_id = item.id;
    for item in recovered(&f)
        .iter()
        .filter(|item| item.side == "remote" && item.id != restored_id)
    {
        assert!(f.remote_root.join(&item.stored_path).exists(), "{item:?}");
    }
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("file.txt"), "local edit");
    assert_eq!(f.read_remote("file.txt"), "local edit");
}

#[test]
fn upload_interrupted_while_preserving_puts_the_remote_version_back() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("other.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    crash_at(&mut f, "preserve_remote:after_move");
    assert!(!f.remote_file("file.txt").exists());
    f.pass();
    f.assert_no_errors();
    assert_eq!(conflicts(&f), 0);
    assert_eq!(f.read_remote("file.txt"), "local edit");
    for item in recovered(&f) {
        assert!(f.remote_root.join(&item.stored_path).exists(), "{item:?}");
    }
    f.assert_clean();
}

#[test]
fn a_local_file_created_during_a_download_is_compared_not_duplicated() {
    let mut f = Fixture::new();
    f.seed("other.txt");
    fs::write(f.remote_file("new.txt"), "remote").unwrap();
    set_time(&f.remote_file("new.txt"), 1_600_000_000);
    let local_file = f.local.join("new.txt");
    crate::sync::failpoint::arm_action("download:staged", move || {
        fs::write(&local_file, "local!").unwrap();
    });
    f.pass();
    crate::sync::failpoint::disarm();
    f.retry_all();
    f.pass();
    f.assert_no_errors_except_conflicts();
    assert_eq!(conflicts(&f), 1);
    let copy = only_conflict_copy(&f, "new");
    assert_eq!(f.read_local(&copy), "remote");
    assert_eq!(f.read_remote(&copy), "remote");
    assert_eq!(f.read_local("new.txt"), "local!");
    assert_eq!(f.read_remote("new.txt"), "local!");
    assert_eq!(staging_entries(&f), 0);
}

/// Hooks that record what a pass reports about its transfers.
#[derive(Default)]
struct Recorder {
    plans: Vec<crate::sync::progress::Plan>,
    started: Vec<String>,
    finished: Vec<(u64, u64)>,
    activity: Vec<(String, String, i64)>,
}

impl pass::Hooks for Recorder {
    fn checkpoint(&mut self) -> bool {
        true
    }

    fn progress(&mut self, _text: String) {}

    fn plan(&mut self, plan: &crate::sync::progress::Plan) {
        self.plans.push(*plan);
    }

    fn item_started(&mut self, rel: &str) {
        self.started.push(rel.to_owned());
    }

    fn item_finished(&mut self, _rel: &str, files: u64, bytes: u64) {
        self.finished.push((files, bytes));
    }

    fn activity(&mut self, activity: &store::Activity) {
        self.activity
            .push((activity.rel.clone(), activity.action.clone(), activity.size));
    }
}

#[test]
fn a_pass_reports_its_plan_transfers_and_recent_changes() {
    let f = Fixture::new();
    fs::write(f.local.join("up.txt"), "12345").unwrap();
    fs::write(f.remote_file("down.txt"), "abc").unwrap();
    fs::create_dir(f.local.join("folder")).unwrap();
    let group = "celestial/test-progress";
    rclone::delete_group_stats(group);
    rclone::sync::set_stats_group(Some(group.into()));
    let mut hooks = Recorder::default();
    let report = pass::run(&f.pair(), &mut hooks, None);
    rclone::sync::set_stats_group(None);
    let report = report.unwrap();
    assert_eq!(report.space, None);
    assert_eq!(
        hooks.plans,
        [crate::sync::progress::Plan {
            upload_files: 1,
            upload_bytes: 5,
            download_files: 1,
            download_bytes: 3,
            ..Default::default()
        }]
    );
    hooks.started.sort();
    assert_eq!(hooks.started, ["down.txt", "up.txt"]);
    assert_eq!(
        hooks.finished.iter().map(|(files, _)| files).sum::<u64>(),
        2
    );
    assert_eq!(
        hooks.finished.iter().map(|(_, bytes)| bytes).sum::<u64>(),
        8
    );
    hooks.activity.sort();
    assert_eq!(
        hooks.activity,
        [
            ("down.txt".into(), "downloaded".into(), 3),
            ("up.txt".into(), "uploaded".into(), 5)
        ]
    );
    // The transfers were counted in the pass's stats group.
    assert!(rclone::group_stats(group).unwrap().bytes >= 8);
    rclone::delete_group_stats(group);
    let stored = store::recent_activity(&f.db, f.dir.id, 10).unwrap();
    assert_eq!(stored.len(), 2);

    // A propagated deletion is a removal in the plan and a recent change.
    fs::remove_file(f.local.join("up.txt")).unwrap();
    let mut hooks = Recorder::default();
    pass::run(&f.pair(), &mut hooks, None).unwrap();
    assert_eq!(hooks.plans[0].removals, 1);
    assert_eq!(hooks.plans[0].files(), 0);
    assert_eq!(
        hooks.activity,
        [("up.txt".into(), "deleted_remote".into(), 5)]
    );
    assert_eq!(
        store::recent_activity(&f.db, f.dir.id, 1).unwrap()[0].action,
        "deleted_remote"
    );
}

#[test]
fn uploads_wait_for_space_unless_approved() {
    use crate::sync::progress::Plan;
    let plan = Plan {
        upload_files: 1,
        upload_bytes: 10 << 20,
        ..Plan::default()
    };
    assert_eq!(
        pass::space_shortage(&plan, Some(5 << 20), false),
        Some(pass::SpaceShortage {
            needed: 10 << 20,
            free: 5 << 20
        })
    );
    assert_eq!(pass::space_shortage(&plan, Some(5 << 20), true), None);
    assert_eq!(pass::space_shortage(&plan, Some(1 << 30), false), None);
    // A remote that does not report its free space is not gated.
    assert_eq!(pass::space_shortage(&plan, None, false), None);
    let downloads = Plan {
        download_files: 1,
        download_bytes: 1 << 30,
        ..Plan::default()
    };
    assert_eq!(pass::space_shortage(&downloads, Some(0), false), None);
}

#[test]
fn stopping_a_pair_forgets_its_recent_changes() {
    let f = Fixture::new();
    store::insert_activity(
        &f.db,
        f.dir.id,
        &store::Activity {
            rel: "a".into(),
            action: "uploaded".into(),
            size: 1,
            finished_at: 1,
        },
    )
    .unwrap();
    let tx = util::await_future(f.db.begin()).unwrap();
    util::await_future(forget_pair_state(&tx, f.dir.id)).unwrap();
    util::await_future(tx.commit()).unwrap();
    assert!(
        store::recent_activity(&f.db, f.dir.id, 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn remote_folder_size_counts_files_and_treats_a_missing_folder_as_empty() {
    let f = Fixture::new();
    fs::create_dir_all(f.remote_file("a")).unwrap();
    fs::write(f.remote_file("one.txt"), "12345").unwrap();
    fs::write(f.remote_file("a/two.txt"), "123").unwrap();
    let size = rclone::sync::folder_size(&f.remote.name, "sandbox").unwrap();
    assert_eq!((size.count, size.bytes), (2, 8));
    let missing = rclone::sync::folder_size(&f.remote.name, "sandbox/not-yet").unwrap();
    assert_eq!(missing, rclone::FolderSize::default());
}

#[test]
fn parallel_copies_synchronize_files_in_new_folders_both_ways() {
    let mut f = Fixture::new();
    for dir in ["up/a", "up/a/b"] {
        fs::create_dir_all(f.local.join(dir)).unwrap();
    }
    for dir in ["down/x", "down/x/y"] {
        fs::create_dir_all(f.remote_file(dir)).unwrap();
    }
    for index in 0..12 {
        fs::write(
            f.local.join(format!("up/a/b/{index}.txt")),
            format!("local {index}"),
        )
        .unwrap();
        fs::write(
            f.local.join(format!("up/{index}.txt")),
            format!("top {index}"),
        )
        .unwrap();
        fs::write(
            f.remote_file(&format!("down/x/y/{index}.txt")),
            format!("remote {index}"),
        )
        .unwrap();
    }
    f.pass();
    f.assert_no_errors();
    for index in 0..12 {
        assert_eq!(
            f.read_remote(&format!("up/a/b/{index}.txt")),
            format!("local {index}")
        );
        assert_eq!(
            f.read_remote(&format!("up/{index}.txt")),
            format!("top {index}")
        );
        assert_eq!(
            f.read_local(&format!("down/x/y/{index}.txt")),
            format!("remote {index}")
        );
    }
    assert_eq!(
        store::recent_activity(&f.db, f.dir.id, 100).unwrap().len(),
        36
    );
    // Every copy left a verified baseline: nothing to do next time.
    f.assert_clean();
    // Edits on both sides, still in parallel.
    for index in 0..6 {
        fs::write(f.local.join(format!("up/{index}.txt")), "edited here").unwrap();
        fs::write(
            f.remote_file(&format!("down/x/y/{index}.txt")),
            "edited there",
        )
        .unwrap();
    }
    f.pass();
    f.assert_no_errors();
    for index in 0..6 {
        assert_eq!(f.read_remote(&format!("up/{index}.txt")), "edited here");
        assert_eq!(
            f.read_local(&format!("down/x/y/{index}.txt")),
            "edited there"
        );
    }
    f.assert_clean();
}

#[test]
fn one_failed_parallel_copy_leaves_the_others_synchronized() {
    if !permissions_apply() {
        return;
    }
    let mut f = Fixture::new();
    for index in 0..8 {
        fs::write(f.local.join(format!("{index}.txt")), format!("{index}")).unwrap();
    }
    let locked = f.local.join("3.txt");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    f.pass();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
    let states = item_states(&f);
    assert_eq!(states.len(), 1, "{states:?}");
    assert_eq!(states["3.txt"].state, store::RETRYING);
    for index in (0..8).filter(|index| *index != 3) {
        assert_eq!(f.read_remote(&format!("{index}.txt")), index.to_string());
    }
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("3.txt"), "3");
}

#[test]
fn copies_commit_their_outcome_once() {
    let mut f = Fixture::new();
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(f.local.join(name), name).unwrap();
    }
    f.pass();
    f.assert_no_errors();
    // New files have nothing to undo after an interruption: no journal.
    let journaled = store::query(&f.db, "SELECT COUNT(*) AS n FROM operations", vec![]).unwrap();
    assert_eq!(journaled[0].try_get::<i64>("", "n").unwrap(), 0);
    let recent = store::recent_activity(&f.db, f.dir.id, 10).unwrap();
    assert_eq!(recent.len(), 3);
    // A download is journaled once, at the staged step, and closed.
    fs::write(f.remote_file("d.txt"), "remote").unwrap();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_local("d.txt"), "remote");
    let states = store::query(&f.db, "SELECT state FROM operations", vec![]).unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(
        states[0].try_get::<String>("", "state").unwrap(),
        "complete"
    );
}

#[test]
fn a_file_still_being_written_is_uploaded_once_it_is_closed() {
    use std::io::Write;
    let mut f = Fixture::new();
    crate::sync::writers::enable_for_test(0);
    let path = f.local.join("recording.mkv");
    let mut file = fs::File::create(&path).unwrap();
    file.write_all(b"header").unwrap();
    f.pass();
    assert!(!f.remote_file("recording.mkv").exists());
    let state = &item_states(&f)["recording.mkv"];
    assert_eq!(state.state, store::RETRYING);
    assert_eq!(state.attempts, 0);
    assert!(state.next_attempt.unwrap() > store::now());
    // Still open when the wait ends: it keeps waiting, without a backoff.
    file.write_all(b" and frames").unwrap();
    f.retry_all();
    f.pass();
    assert!(!f.remote_file("recording.mkv").exists());
    assert_eq!(item_states(&f)["recording.mkv"].attempts, 0);
    file.write_all(b" and the end").unwrap();
    drop(file);
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(
        f.read_remote("recording.mkv"),
        "header and frames and the end"
    );
    assert!(item_states(&f).is_empty());
}

#[test]
fn a_synced_file_rewritten_in_place_keeps_its_remote_version_until_done() {
    use std::io::Write;
    let mut f = Fixture::new();
    f.seed("video.mkv");
    crate::sync::writers::enable_for_test(0);
    let mut file = fs::File::create(f.local.join("video.mkv")).unwrap();
    file.write_all(b"partial").unwrap();
    f.pass();
    assert_eq!(f.read_remote("video.mkv"), "original");
    drop(file);
    f.retry_all();
    f.pass();
    f.assert_no_errors();
    assert_eq!(f.read_remote("video.mkv"), "partial");
}

#[test]
fn a_just_modified_file_waits_for_the_quiet_age() {
    let mut f = Fixture::new();
    crate::sync::writers::enable_for_test(crate::sync::writers::QUIET_SECS);
    fs::write(f.local.join("fresh.txt"), "fresh").unwrap();
    f.pass();
    assert!(!f.remote_file("fresh.txt").exists());
    let state = &item_states(&f)["fresh.txt"];
    assert_eq!(state.state, store::RETRYING);
    assert!(state.next_attempt.unwrap() <= store::now() + crate::sync::writers::QUIET_SECS + 1);
}

#[test]
fn deleting_all_preserved_versions_empties_the_list() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    for edit in ["one", "two", "three"] {
        fs::write(f.remote_file("file.txt"), edit).unwrap();
        set_time(&f.remote_file("file.txt"), 1_700_000_000);
        f.pass();
    }
    let pair = f.pair();
    assert!(!pass::recovery_list(&pair).unwrap().is_empty());
    let (deleted, kept) = pass::discard_all(&pair).unwrap();
    assert!(deleted > 0);
    assert_eq!(kept, 0);
    assert!(pass::recovery_list(&pair).unwrap().is_empty());
}

#[test]
fn unavailable_local_root_waits_before_reporting() {
    use super::{local_root_returned, waiting_for_local_root};
    use std::time::Duration;
    let id = -4242;
    assert!(waiting_for_local_root(id, Duration::from_secs(60)));
    assert!(!waiting_for_local_root(id, Duration::ZERO));
    local_root_returned(id);
    assert!(waiting_for_local_root(id, Duration::from_secs(60)));
    local_root_returned(id);
}
