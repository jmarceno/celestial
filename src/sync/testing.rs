//! Test fixture: a pair between a temporary local folder and a temporary
//! folder behind an rclone `alias` remote. No cloud account is involved.

use super::capabilities::{self, Capabilities};
use super::model::{self, Model, SyncError};
use super::observe;
use super::pass::{self, Hooks, Pair, Report};
use super::{Shared, store};
use crate::entities::{RemotesActiveModel, RemotesModel, SyncDirsActiveModel, SyncDirsModel};
use crate::migrations::{Migrator, MigratorTrait};
use crate::util;
use sea_orm::{ActiveModelTrait, ActiveValue, Database, DatabaseConnection};
use serde_json::json;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Once};
use std::time::{Duration, SystemTime};

static RCLONE_LOCK: Mutex<()> = Mutex::new(());
static RCLONE_INIT: Once = Once::new();

/// Lock the global rclone configuration for one test.
pub fn rclone_guard() -> MutexGuard<'static, ()> {
    let guard = RCLONE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
    RCLONE_INIT.call_once(librclone::initialize);
    guard
}

pub struct Fixture {
    _guard: MutexGuard<'static, ()>,
    pub temp: tempfile::TempDir,
    pub local: PathBuf,
    pub remote_root: PathBuf,
    pub remote: RemotesModel,
    pub dir: SyncDirsModel,
    pub db: DatabaseConnection,
    pub model: Model,
    pub shared: Shared,
    pub caps: Capabilities,
    pub last: Option<Report>,
}

/// Hooks that run a closure at each checkpoint, to change things between
/// observation and action.
pub struct TestHooks<'a> {
    pub calls: usize,
    pub on_checkpoint: Option<Box<dyn FnMut(usize) + 'a>>,
}

impl Hooks for TestHooks<'_> {
    fn checkpoint(&mut self) -> bool {
        self.calls += 1;
        if let Some(action) = self.on_checkpoint.as_mut() {
            action(self.calls);
        }
        true
    }

    fn progress(&mut self, _text: String) {}
}

impl Fixture {
    pub fn new() -> Self {
        let guard = rclone_guard();
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
        let caps = capabilities::probe(&db, &remote.name, "test").unwrap();
        // Files written by a test are seconds old at most; only the racy-clean
        // test uses the real window.
        observe::set_racy_window(0);
        Self {
            _guard: guard,
            temp,
            local,
            remote_root,
            remote,
            dir,
            db,
            model,
            shared,
            caps,
            last: None,
        }
    }

    pub fn pair(&self) -> Pair<'_> {
        Pair {
            db: &self.db,
            remote: &self.remote,
            dir: &self.dir,
            caps: &self.caps,
        }
    }

    /// Run a pass the way the worker does, recording its outcome in the model.
    pub fn pass(&mut self) {
        self.pass_with(|_| {});
    }

    pub fn pass_with(&mut self, action: impl FnMut(usize)) {
        if !super::engine::pair_roots_available(
            &self.db,
            &self.remote,
            &self.dir,
            &mut self.model,
            &self.shared,
        ) {
            return;
        }
        let mut hooks = TestHooks {
            calls: 0,
            on_checkpoint: Some(Box::new(action)),
        };
        let result = pass::run(&self.pair(), &mut hooks, None);
        let (remote, local, remote_path) = self.names();
        match result {
            Ok(report) => {
                self.model
                    .set_pair_error(&remote, &local, &remote_path, None);
                self.model
                    .set_issues(&remote, &local, &remote_path, report.issues.clone());
                self.last = Some(report);
            }
            Err(message) => {
                self.model.set_pair_error(
                    &remote,
                    &local,
                    &remote_path,
                    Some(SyncError::General(local.clone(), message)),
                );
                self.last = None;
            }
        }
    }

    /// Make every waiting item due, as after a dismissal.
    pub fn retry_all(&self) {
        store::exec(&self.db, "UPDATE item_states SET next_attempt = 0", vec![]).unwrap();
    }

    /// Run a pass and require that it changed nothing.
    pub fn assert_clean(&mut self) {
        self.pass();
        self.assert_no_errors();
        let report = self.last.as_ref().expect("pass report");
        assert_eq!(report.actions, 0, "{report:?}");
    }

    pub fn names(&self) -> (String, String, String) {
        (
            self.remote.name.clone(),
            self.dir.local_path.clone(),
            self.dir.remote_path.clone(),
        )
    }

    pub fn errors(&self) -> Vec<model::StoredError> {
        let (remote, local, remote_path) = self.names();
        self.model
            .dir(&remote, &local, &remote_path)
            .map(|dir| dir.errors.clone())
            .unwrap_or_default()
    }

    pub fn assert_no_errors(&self) {
        assert_eq!(self.model.error_dir_count(), 0, "{:?}", self.errors());
    }

    /// No problems besides informational conflict records.
    pub fn assert_no_errors_except_conflicts(&self) {
        let errors: Vec<_> = self
            .errors()
            .into_iter()
            .filter(|err| !matches!(err.error, SyncError::Conflict { .. }))
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    pub fn seed(&mut self, name: &str) {
        fs::write(self.local.join(name), "original").unwrap();
        set_time(&self.local.join(name), 1_700_000_000);
        self.pass();
        self.assert_no_errors();
    }

    pub fn remote_file(&self, rel: &str) -> PathBuf {
        self.remote_root.join("sandbox").join(rel)
    }

    pub fn read_local(&self, rel: &str) -> String {
        fs::read_to_string(self.local.join(rel)).unwrap()
    }

    pub fn read_remote(&self, rel: &str) -> String {
        fs::read_to_string(self.remote_file(rel)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        observe::set_racy_window(2_000_000_000);
    }
}

pub fn set_time(path: &Path, timestamp: u64) {
    File::open(path)
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(timestamp)),
        )
        .unwrap();
}

/// Whether permission bits stop this process (they do not stop root).
pub fn permissions_apply() -> bool {
    !nix::unistd::geteuid().is_root()
}
