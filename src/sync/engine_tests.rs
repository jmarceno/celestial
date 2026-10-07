//! All rclone operations here target disposable local directories, never cloud accounts.
use super::*;
use crate::entities::RemotesActiveModel;
use sea_orm::ConnectionTrait;
use std::sync::{Arc, Mutex, MutexGuard, Once};

static RCLONE_LOCK: Mutex<()> = Mutex::new(());
static RCLONE_INIT: Once = Once::new();

struct Fixture {
    _guard: MutexGuard<'static, ()>,
    _temp: tempfile::TempDir,
    local: PathBuf,
    remote_root: PathBuf,
    remote: RemotesModel,
    dir: SyncDirsModel,
    db: DatabaseConnection,
    model: Model,
    shared: Shared,
    rx: crossbeam_channel::Receiver<Command>,
    queue: VecDeque<Command>,
}

impl Fixture {
    fn new() -> Self {
        let guard = RCLONE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        RCLONE_INIT.call_once(librclone::initialize);
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let remote_root = temp.path().join("remote");
        fs::create_dir(&local).unwrap();
        fs::create_dir_all(remote_root.join("sandbox")).unwrap();
        librclone::rpc(
            "config/setpath",
            json!({"path": temp.path().join("rclone.conf")}).to_string(),
        )
        .unwrap();
        let remote_name = format!(
            "test-sync-{}",
            temp.path().file_name().unwrap().to_str().unwrap()
        );
        librclone::rpc(
            "config/create",
            json!({
                "name": remote_name, "type": "alias", "parameters": {"remote": remote_root}
            })
            .to_string(),
        )
        .unwrap();
        let db = util::await_future(Database::connect("sqlite::memory:")).unwrap();
        util::await_future(Migrator::up(&db, None)).unwrap();
        let remote = util::await_future(
            RemotesActiveModel {
                name: ActiveValue::Set(remote_name),
                ..Default::default()
            }
            .insert(&db),
        )
        .unwrap();
        let dir = util::await_future(
            SyncDirsActiveModel {
                remote_id: ActiveValue::Set(remote.id),
                local_path: ActiveValue::Set(local.to_str().unwrap().into()),
                remote_path: ActiveValue::Set("sandbox".into()),
                ..Default::default()
            }
            .insert(&db),
        )
        .unwrap();
        let (notices, _) = crossbeam_channel::unbounded();
        let (_, rx) = crossbeam_channel::unbounded();
        let shared = Shared {
            bus: model::SnapshotBus::new(),
            quit: Arc::default(),
            stopped: Arc::default(),
            cancel_auth: Arc::default(),
            notices,
            tray: Arc::new(|_, _| {}),
        };
        let mut model = Model::new();
        model.ensure_dir(&remote.name, &dir.local_path, &dir.remote_path);
        Self {
            _guard: guard,
            _temp: temp,
            local,
            remote_root,
            remote,
            dir,
            db,
            model,
            shared,
            rx,
            queue: VecDeque::new(),
        }
    }

    fn pass(&mut self) {
        if !pair_roots_available(
            &self.db,
            &self.remote,
            &self.dir,
            &mut self.model,
            &self.shared,
        ) {
            return;
        }
        let synced = RefCell::new(Default::default());
        sync_local_directory(
            &self.local,
            &self.remote,
            &self.dir,
            &self.db,
            &mut self.model,
            &self.shared,
            &synced,
            &self.rx,
            &mut self.queue,
            "resolve",
        );
        if !self.model.has_errors(
            &self.remote.name,
            &self.dir.local_path,
            &self.dir.remote_path,
        ) {
            sync_remote_directory(
                &self.dir.remote_path,
                &self.remote,
                &self.dir,
                &self.db,
                &mut self.model,
                &self.shared,
                &synced,
                &self.rx,
                &mut self.queue,
                "resolve",
            );
        }
    }

    /// Probe the pair the way the worker does, then pass only when it changed.
    fn probed_pass(&mut self) {
        match directory_changed(&self.db, &self.remote, &self.dir) {
            Ok(true) => self.pass(),
            Ok(false) => {}
            Err(err) => record_error(
                &mut self.model,
                &self.shared,
                &self.remote.name,
                &self.dir,
                SyncError::General(self.dir.local_path.clone(), err),
            ),
        }
    }

    fn assert_no_errors(&self) {
        assert_eq!(self.model.error_dir_count(), 0, "{}", self.model.snapshot());
    }

    fn seed(&mut self, name: &str) {
        fs::write(self.local.join(name), "original").unwrap();
        set_time(&self.local.join(name), 1_700_000_000);
        self.pass();
        self.assert_no_errors();
    }
}

fn set_time(path: &Path, timestamp: u64) {
    File::open(path)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)),
        )
        .unwrap();
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
    assert_eq!(
        fs::read_to_string(fixture.remote_root.join("sandbox/neighbor.txt")).unwrap(),
        "neighbor"
    );
    assert!(!fixture.remote_root.join("sandbox/victim.txt").exists());
}

#[test]
fn failed_upload_preserves_existing_remote_copy() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let item = fs::read_dir(&f.local).unwrap().next().unwrap().unwrap();
    let local_path = item.path().to_str().unwrap().to_owned();
    let remote_item = rclone::sync::stat(&f.remote.name, "sandbox/file.txt")
        .unwrap()
        .unwrap();
    let file_type = item.file_type().ok();
    fs::remove_file(item.path()).unwrap();
    let synced = RefCell::new(Default::default());
    assert!(
        push_local(
            file_type,
            &item,
            &local_path,
            "sandbox/file.txt",
            &f.remote,
            &f.dir,
            &f.db,
            &mut f.model,
            &f.shared,
            &synced,
            &f.rx,
            &mut f.queue,
            "resolve",
            Some(&remote_item)
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "original"
    );
}

#[test]
fn failed_download_preserves_existing_local_copy() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let item = fs::read_dir(&f.local).unwrap().next().unwrap().unwrap();
    let local_path = item.path().to_str().unwrap().to_owned();
    let remote_item = rclone::sync::stat(&f.remote.name, "sandbox/file.txt")
        .unwrap()
        .unwrap();
    fs::remove_file(f.remote_root.join("sandbox/file.txt")).unwrap();
    let synced = RefCell::new(Default::default());
    assert!(
        pull_local(
            item.file_type().ok(),
            &item,
            &local_path,
            "sandbox/file.txt",
            &f.remote,
            &f.dir,
            &f.db,
            &mut f.model,
            &f.shared,
            &synced,
            &f.rx,
            &mut f.queue,
            "resolve",
            Some(&remote_item)
        )
        .is_err()
    );
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "original"
    );
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
    assert!(directory_changed(&f.db, &f.remote, &f.dir).unwrap());
    f.pass();
    f.assert_no_errors();
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/nested/file.txt")).unwrap(),
        "edited locally"
    );
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
    assert_eq!(
        fs::read_to_string(f.local.join("nested/file.txt")).unwrap(),
        "edited remotely"
    );
}

#[test]
fn unchanged_nonroot_pair_is_detected_as_clean() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn new_download_records_actual_local_timestamp() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "old").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_000);
    fs::write(f.remote_root.join("sandbox/file.txt"), "remote newer").unwrap();
    set_time(&f.remote_root.join("sandbox/file.txt"), 1_700_000_100);
    f.pass();
    f.assert_no_errors();
    let row = util::await_future(SyncItemsEntity::find().one(&f.db))
        .unwrap()
        .unwrap();
    let actual = fs::metadata(f.local.join("file.txt"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(u64::try_from(row.last_local_timestamp).unwrap(), actual);
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
    f.queue.push_back(Command::StopDir {
        remote: other.name,
        local: f.dir.local_path.clone(),
        remote_path: f.dir.remote_path.clone(),
    });
    apply_deletes(&mut f.model, &f.db, &mut f.queue, &f.shared);
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
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "original"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn unavailable_remote_root_preserves_local_files() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_dir_all(f.remote_root.join("sandbox")).unwrap();
    f.pass();
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "original"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn remote_folder_deletion_preserves_changed_local_children() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("nested")).unwrap();
    f.seed("nested/file.txt");
    fs::remove_dir_all(f.remote_root.join("sandbox/nested")).unwrap();
    fs::write(f.local.join("nested/file.txt"), "unsynced edit").unwrap();
    set_time(&f.local.join("nested/file.txt"), 1_700_000_100);
    f.pass();
    assert_eq!(
        fs::read_to_string(f.local.join("nested/file.txt")).unwrap(),
        "unsynced edit"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn local_folder_deletion_preserves_changed_remote_children() {
    let mut f = Fixture::new();
    fs::create_dir(f.local.join("nested")).unwrap();
    f.seed("nested/file.txt");
    fs::remove_dir_all(f.local.join("nested")).unwrap();
    fs::write(
        f.remote_root.join("sandbox/nested/file.txt"),
        "unsynced remote edit",
    )
    .unwrap();
    set_time(
        &f.remote_root.join("sandbox/nested/file.txt"),
        1_700_000_100,
    );
    f.pass();
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/nested/file.txt")).unwrap(),
        "unsynced remote edit"
    );
    assert_eq!(f.model.error_dir_count(), 1);
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
        assert_eq!(
            fs::read_to_string(f.local.join("neighbor.txt")).unwrap(),
            "original"
        );
        assert_eq!(
            fs::read_to_string(f.remote_root.join("sandbox/neighbor.txt")).unwrap(),
            "original"
        );
    }
}

#[test]
fn exclusions_apply_on_both_sides_of_nonroot_pair() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "*.tmp\n").unwrap();
    fs::write(f.local.join("local.tmp"), "excluded locally").unwrap();
    fs::write(
        f.remote_root.join("sandbox/remote.tmp"),
        "excluded remotely",
    )
    .unwrap();
    f.seed("file.txt");
    assert!(!f.local.join("remote.tmp").exists());
    assert!(!f.remote_root.join("sandbox/local.tmp").exists());
    assert!(
        !f.remote_root
            .join("sandbox")
            .join(FILE_IGNORE_NAME)
            .exists()
    );
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn invalid_exclusions_pause_transfers() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "[invalid").unwrap();
    fs::write(f.local.join("file.txt"), "keep local").unwrap();
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert!(!f.remote_root.join("sandbox/file.txt").exists());
}

#[test]
fn symlink_inside_local_pair_cannot_overwrite_outside_file() {
    let mut f = Fixture::new();
    let outside = f._temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("file.txt"), "outside data").unwrap();
    std::os::unix::fs::symlink(&outside, f.local.join("linked")).unwrap();
    fs::create_dir(f.remote_root.join("sandbox/linked")).unwrap();
    fs::write(f.remote_root.join("sandbox/linked/file.txt"), "remote data").unwrap();
    let synced = RefCell::new(Default::default());
    sync_remote_directory(
        &f.dir.remote_path,
        &f.remote,
        &f.dir,
        &f.db,
        &mut f.model,
        &f.shared,
        &synced,
        &f.rx,
        &mut f.queue,
        "resolve",
    );
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
    util::await_future(f.db.execute(sea_orm::Statement::from_string(
        sea_orm::DbBackend::Sqlite,
        "DROP TABLE sync_items".to_owned(),
    )))
    .unwrap();
    f.pass();
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "original"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn database_write_failure_is_reported() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local edit").unwrap();
    set_time(&f.local.join("file.txt"), 1_700_000_100);
    util::await_future(f.db.execute(sea_orm::Statement::from_string(
        sea_orm::DbBackend::Sqlite,
        "PRAGMA query_only = ON".to_owned(),
    )))
    .unwrap();
    f.pass();
    assert_eq!(f.model.error_dir_count(), 1);
    let row = util::await_future(SyncItemsEntity::find().one(&f.db))
        .unwrap()
        .unwrap();
    assert_eq!(row.last_local_timestamp, 1_700_000_000);
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
    let rows = util::await_future(SyncItemsEntity::find().all(&f.db)).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .find(|row| row.sync_dir_id == other_dir.id)
            .unwrap()
            .last_local_timestamp,
        1_800_000_000
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.sync_dir_id == f.dir.id)
            .unwrap()
            .last_local_timestamp,
        1_700_000_000
    );
}

#[test]
fn remote_update_of_existing_record_does_not_insert_duplicate() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    fs::write(f.remote_root.join("sandbox/file.txt"), "new remote edit").unwrap();
    set_time(&f.remote_root.join("sandbox/file.txt"), 1_700_000_100);
    f.pass();
    f.assert_no_errors();
    assert_eq!(
        util::await_future(SyncItemsEntity::find().all(&f.db))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "new remote edit"
    );
}

#[test]
fn history_for_items_missing_on_both_sides_is_retired_after_complete_probe() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    fs::remove_file(f.remote_root.join("sandbox/file.txt")).unwrap();
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
    assert!(
        util::await_future(SyncItemsEntity::find().all(&f.db))
            .unwrap()
            .is_empty()
    );
    // Recreating the same pathname and timestamp must not delete the new file.
    f.seed("file.txt");
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "original"
    );
}

#[test]
fn failed_probe_preserves_history() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    let mut broken = f.remote.clone();
    broken.name = "nonexistent-test-remote".into();
    assert!(directory_changed(&f.db, &broken, &f.dir).is_err());
    assert_eq!(
        util::await_future(SyncItemsEntity::find().all(&f.db))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn failed_stop_command_preserves_pair_and_history() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    util::await_future(f.db.execute(sea_orm::Statement::from_string(
        sea_orm::DbBackend::Sqlite,
        "PRAGMA query_only = ON".to_owned(),
    )))
    .unwrap();
    f.queue.push_back(Command::StopDir {
        remote: f.remote.name.clone(),
        local: f.dir.local_path.clone(),
        remote_path: f.dir.remote_path.clone(),
    });
    apply_deletes(&mut f.model, &f.db, &mut f.queue, &f.shared);
    assert!(f.dir.exists(&f.db).unwrap());
    assert_eq!(
        util::await_future(SyncItemsEntity::find().all(&f.db))
            .unwrap()
            .len(),
        1
    );
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
    f.probed_pass();
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "original"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn emptied_remote_root_preserves_local_files() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.remote_root.join("sandbox/file.txt")).unwrap();
    f.probed_pass();
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "original"
    );
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn partial_deletion_still_propagates() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    fs::remove_file(f.local.join("file.txt")).unwrap();
    f.probed_pass();
    f.assert_no_errors();
    assert!(!f.remote_root.join("sandbox/file.txt").exists());
}

#[test]
fn remote_exclusion_file_never_replaces_local_rules() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "*.tmp\n").unwrap();
    set_time(&f.local.join(FILE_IGNORE_NAME), 1_700_000_000);
    let remote_rules = f.remote_root.join("sandbox").join(FILE_IGNORE_NAME);
    fs::write(&remote_rules, "*.other\n").unwrap();
    set_time(&remote_rules, 1_700_000_100);
    f.seed("file.txt");
    assert_eq!(
        fs::read_to_string(f.local.join(FILE_IGNORE_NAME)).unwrap(),
        "*.tmp\n"
    );
    assert_eq!(fs::read_to_string(&remote_rules).unwrap(), "*.other\n");
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn restored_older_remote_version_is_downloaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.remote_root.join("sandbox/file.txt"), "restored").unwrap();
    set_time(&f.remote_root.join("sandbox/file.txt"), 1_600_000_000);
    f.probed_pass();
    f.assert_no_errors();
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "restored"
    );
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn restored_older_local_version_is_uploaded() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "restored").unwrap();
    set_time(&f.local.join("file.txt"), 1_600_000_000);
    f.probed_pass();
    f.assert_no_errors();
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "restored"
    );
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn older_versions_restored_on_both_sides_are_a_conflict() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::write(f.local.join("file.txt"), "local restore").unwrap();
    set_time(&f.local.join("file.txt"), 1_600_000_000);
    fs::write(f.remote_root.join("sandbox/file.txt"), "remote restore").unwrap();
    set_time(&f.remote_root.join("sandbox/file.txt"), 1_600_000_100);
    f.probed_pass();
    assert_eq!(f.model.error_dir_count(), 1);
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "local restore"
    );
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "remote restore"
    );
}

#[test]
fn timestamps_after_2038_are_recorded() {
    let mut f = Fixture::new();
    fs::write(f.local.join("file.txt"), "future").unwrap();
    set_time(&f.local.join("file.txt"), 4_000_000_000);
    f.probed_pass();
    f.assert_no_errors();
    let row = util::await_future(SyncItemsEntity::find().one(&f.db))
        .unwrap()
        .unwrap();
    assert_eq!(row.last_local_timestamp, 4_000_000_000);
    assert_eq!(row.last_remote_timestamp, 4_000_000_000);
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

impl Fixture {
    fn conflict(&mut self) -> u64 {
        self.seed("file.txt");
        fs::write(self.local.join("file.txt"), "local edit").unwrap();
        set_time(&self.local.join("file.txt"), 1_700_000_100);
        let remote_file = self.remote_root.join("sandbox/file.txt");
        fs::write(&remote_file, "remote edit").unwrap();
        set_time(&remote_file, 1_700_000_200);
        self.probed_pass();
        let errors = &self.model.remotes[&self.remote.name]
            [&(self.dir.local_path.clone(), self.dir.remote_path.clone())]
            .errors;
        assert_eq!(errors.len(), 1);
        errors[0].id
    }

    fn activate(&mut self, id: u64) {
        let (name, local, remote_path) = self.pair();
        activate_error(
            &self.shared,
            &self.db,
            &mut self.model,
            &name,
            &local,
            &remote_path,
            id,
            "resolve",
        );
    }

    fn choose(&mut self, id: u64, keep_local: bool) {
        let (name, local, remote_path) = self.pair();
        choose_conflict(
            &self.shared,
            &self.db,
            &mut self.model,
            &name,
            &local,
            &remote_path,
            id,
            keep_local,
            "resolve",
        );
    }

    fn pair(&self) -> (String, String, String) {
        (
            self.remote.name.clone(),
            self.dir.local_path.clone(),
            self.dir.remote_path.clone(),
        )
    }
}

#[test]
fn conflict_choice_copies_the_versions_shown() {
    let mut f = Fixture::new();
    let id = f.conflict();
    f.activate(id);
    f.choose(id, true);
    f.assert_no_errors();
    assert_eq!(
        fs::read_to_string(f.remote_root.join("sandbox/file.txt")).unwrap(),
        "local edit"
    );
    assert!(!directory_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn conflict_choice_is_refused_after_an_item_changes() {
    let mut f = Fixture::new();
    let id = f.conflict();
    f.activate(id);
    let remote_file = f.remote_root.join("sandbox/file.txt");
    fs::write(&remote_file, "newer remote edit").unwrap();
    set_time(&remote_file, 1_700_000_300);
    f.choose(id, true);
    assert_eq!(
        fs::read_to_string(&remote_file).unwrap(),
        "newer remote edit"
    );
    assert_eq!(f.model.error_dir_count(), 1);
    // Reviewing again shows the current versions, which can then be chosen.
    f.activate(id);
    f.choose(id, true);
    f.assert_no_errors();
    assert_eq!(fs::read_to_string(&remote_file).unwrap(), "local edit");
}

#[test]
fn conflict_choice_without_a_prompt_does_nothing() {
    let mut f = Fixture::new();
    let id = f.conflict();
    f.choose(id, false);
    assert_eq!(f.model.error_dir_count(), 1);
    assert_eq!(
        fs::read_to_string(f.local.join("file.txt")).unwrap(),
        "local edit"
    );
}

#[test]
fn conflict_choice_refuses_a_symlinked_local_item() {
    let mut f = Fixture::new();
    let id = f.conflict();
    f.activate(id);
    let outside = f._temp.path().join("outside.txt");
    fs::write(&outside, "outside data").unwrap();
    fs::remove_file(f.local.join("file.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, f.local.join("file.txt")).unwrap();
    f.choose(id, false);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside data");
    assert_eq!(f.model.error_dir_count(), 1);
}

#[test]
fn remote_probe_detects_remote_edits_and_deletions() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    f.seed("neighbor.txt");
    assert!(!remote_changed(&f.db, &f.remote, &f.dir).unwrap());
    let remote_file = f.remote_root.join("sandbox/file.txt");
    fs::write(&remote_file, "remote edit").unwrap();
    set_time(&remote_file, 1_700_000_100);
    assert!(remote_changed(&f.db, &f.remote, &f.dir).unwrap());
    fs::remove_file(&remote_file).unwrap();
    assert!(remote_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn remote_probe_detects_new_items_and_skips_excluded_ones() {
    let mut f = Fixture::new();
    fs::write(f.local.join(FILE_IGNORE_NAME), "*.tmp\n").unwrap();
    f.seed("file.txt");
    fs::write(f.remote_root.join("sandbox/scratch.tmp"), "excluded").unwrap();
    assert!(!remote_changed(&f.db, &f.remote, &f.dir).unwrap());
    fs::write(f.remote_root.join("sandbox/new.txt"), "new").unwrap();
    assert!(remote_changed(&f.db, &f.remote, &f.dir).unwrap());
}

#[test]
fn remote_probe_pauses_when_the_remote_is_emptied() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    fs::remove_file(f.remote_root.join("sandbox/file.txt")).unwrap();
    assert!(remote_changed(&f.db, &f.remote, &f.dir).is_err());
}

#[test]
fn watched_pair_skips_the_local_scan_until_an_event_arrives() {
    let mut f = Fixture::new();
    f.seed("file.txt");
    let mut watcher = watch::LocalWatcher::new();
    watcher.sync_pairs(&[(f.dir.id, f.local.clone())]);
    // First check scans and verifies the local tree.
    assert!(
        !pair_changed(
            &f.db,
            &f.remote,
            &f.dir,
            watcher.local_unchanged(f.dir.id, &f.local),
            &mut watcher
        )
        .unwrap()
    );
    // A change the watcher cannot see (simulated by editing the history) is
    // not rescanned while the pair is quiet...
    util::await_future(f.db.execute(sea_orm::Statement::from_string(
        sea_orm::DbBackend::Sqlite,
        "UPDATE sync_items SET last_local_timestamp = 1".to_owned(),
    )))
    .unwrap();
    assert!(
        !pair_changed(
            &f.db,
            &f.remote,
            &f.dir,
            watcher.local_unchanged(f.dir.id, &f.local),
            &mut watcher
        )
        .unwrap()
    );
    // ...but a local event triggers a full scan.
    fs::write(f.local.join("other.txt"), "new").unwrap();
    let mut changed = false;
    for _ in 0..40 {
        thread::sleep(Duration::from_millis(50));
        if pair_changed(
            &f.db,
            &f.remote,
            &f.dir,
            watcher.local_unchanged(f.dir.id, &f.local),
            &mut watcher,
        )
        .unwrap()
        {
            changed = true;
            break;
        }
    }
    assert!(changed);
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
    let caps = capabilities::probe(&f.db, &f.remote.name, "test").unwrap();
    assert_eq!(caps.preferred_hash(), Some("md5"));
    assert_eq!(
        store::load_capabilities(&f.db, &f.remote.name).unwrap(),
        Some(caps)
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
    assert!(rclone::sync::stat("bad:", "x").is_err());
    assert!(rclone::sync::list("", "x", false, RcloneListFilter::All).is_err());
}
