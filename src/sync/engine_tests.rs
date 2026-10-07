//! All rclone operations here target disposable local directories, never cloud accounts.
use super::*;
use crate::entities::{RemotesActiveModel, SyncItemsActiveModel};
use crate::sync::model::FILE_IGNORE_NAME;
use crate::sync::recovery;
use crate::sync::testing::{Fixture, TestHooks, permissions_apply, set_time};
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
fn invalid_exclusions_pause_transfers() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "[invalid").unwrap();
    fs::write(f.local.join("file.txt"), "keep local").unwrap();
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert!(!f.remote_file("file.txt").exists());
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
fn retention_keeps_recent_versions_and_the_only_copy_of_deleted_items() {
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
    let rec = recovery::Store {
        db: &f.db,
        remote: &f.remote,
        dir: &f.dir,
        internal: &internal,
    };
    let live = ["live.txt".to_owned()].into();
    rec.prune(&live).unwrap();
    // Ten newest versions of a live item remain.
    assert_eq!(recent.iter().filter(|path| path.exists()).count(), 10);
    assert!(!recent[0].exists() && !recent[1].exists());
    // The only copy of a deleted item is kept even when old.
    assert!(expired.exists());
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
