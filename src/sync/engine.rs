//! Two-way synchronization worker. The Qt window communicates through [`Model`].

use super::capabilities::{self, Capabilities};
use super::model::{self, Model, SyncError};
use super::observe::LocalTree;
use super::pass::{self, Pair};
use super::{Command, Shared, store, watch};
use crate::{
    entities::{
        RemotesColumn, RemotesEntity, RemotesModel, SyncDirsActiveModel, SyncDirsColumn,
        SyncDirsEntity, SyncDirsModel, SyncItemsColumn, SyncItemsEntity,
    },
    login::{self, LoginRequest},
    migrations::{Migrator, MigratorTrait},
    rclone::{self, RcloneListFilter},
    util,
};
use sea_orm::{ActiveValue, Database, DatabaseConnection, Statement, entity::prelude::*};
use sea_orm::{ConnectionTrait, DatabaseTransaction, TransactionTrait};
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

pub fn spawn(
    shared: Shared,
    rx: crossbeam_channel::Receiver<Command>,
) -> std::thread::JoinHandle<()> {
    thread::Builder::new()
        .name("celestial-sync".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&shared, rx)));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(message)) => {
                    tracing::error!("sync worker could not start: {message}");
                    shared.dialog(
                        "fatal",
                        tr::tr!("Unable to start synchronization"),
                        message,
                    );
                }
                Err(payload) => {
                    let message = panic_message(payload);
                tracing::error!("sync worker panicked: {message}");
                shared.dialog(
                    "fatal",
                    tr::tr!("Unknown Error"),
                    format!(
                        "{}\n\n{message}",
                        tr::tr!("An unknown error has occurred while running. This is an internal issue with Celestial and should be reported.\n\nThe following backtrace may help with debugging the issue - note that it may contain information such as login tokens/keys, so avoid posting the information publicly:")
                    ),
                );
                }
            }
            shared.stopped.store(true, Ordering::Relaxed);
        })
        .expect("spawn sync worker")
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else {
        "sync worker panicked".into()
    }
}

/// Worker state that outlives a single pass.
#[derive(Default)]
struct Schedule {
    remote_checks: RemoteChecks,
    /// When the earliest item retry is due, by pair.
    retries: HashMap<i32, i64>,
    /// Local observations a quiet watcher vouches for, by pair.
    local_trees: HashMap<i32, LocalTree>,
}

impl Schedule {
    /// Check a pair on the next loop (after a dismissal or a new rule).
    fn wake(&mut self, id: i32) {
        self.remote_checks.forget(id);
        self.retries.insert(id, store::now());
    }
}

fn worker(shared: &Shared, rx: crossbeam_channel::Receiver<Command>) -> Result<(), String> {
    util::migrate_legacy_config().map_err(|err| {
        tr::tr!(
            "Unable to move the configuration from Celeste's former folder [{}].",
            err
        )
    })?;
    let mut config = util::get_config_dir();
    fs::create_dir_all(&config)
        .map_err(|err| tr::tr!("Unable to create Celestial's config directory [{}].", err))?;
    config.push("rclone.conf");
    librclone::initialize();
    librclone::rpc(
        "config/setpath",
        json!({ "path": config.display().to_string() }).to_string(),
    )
    .map_err(|err| format!("Could not set the rclone config path: {err}"))?;

    let mut db_path = util::get_config_dir();
    db_path.push(util::DATABASE_NAME);
    if !db_path.exists() {
        File::create(&db_path)
            .map_err(|err| tr::tr!("Unable to create Celestial's database file [{}].", err))?;
    }
    let db = util::await_future(Database::connect(format!("sqlite://{}", db_path.display())))
        .map_err(|err| tr::tr!("Unable to connect to database [{}].", err))?;
    backup_before_migration(&db, &db_path)?;
    util::await_future(Migrator::up(&db, None))
        .map_err(|err| tr::tr!("Unable to run database migrations [{}]", err))?;
    let rclone_version = match rclone::version() {
        Ok(version) => version,
        Err(err) => {
            tracing::warn!("could not read the embedded rclone version: {err}");
            "unknown".into()
        }
    };
    tracing::info!("embedded rclone {rclone_version}");

    let mut model = Model::new();
    if let Err(err) = reload_model(&db, &mut model) {
        shared.dialog(
            "error",
            tr::tr!("Unable to read the synchronization settings"),
            err.to_string(),
        );
    }
    shared.publish(&mut model);
    let mut capabilities: HashMap<String, Capabilities> = HashMap::new();

    let mut queue = VecDeque::new();
    let mut prompted_login = false;
    let mut watcher = watch::LocalWatcher::new();
    let mut schedule = Schedule::default();

    loop {
        pump(&rx, &mut queue);
        if shared.quit_requested() {
            break;
        }
        apply_deletes(&mut model, &db, &mut queue, shared);
        shared.publish(&mut model);
        apply_between_passes(
            shared,
            &db,
            &mut model,
            &mut queue,
            &mut schedule,
            &mut capabilities,
        );
        if shared.quit_requested() {
            break;
        }

        // A failed read is not an empty configuration: keep the last known
        // state, report the failure and try again.
        let remotes = match util::await_future(RemotesEntity::find().all(&db)) {
            Ok(remotes) => remotes,
            Err(err) => {
                database_unavailable(shared, &mut model, &err);
                wait_command(&rx, &mut queue, Duration::from_secs(5));
                continue;
            }
        };
        if remotes.is_empty() {
            model.remotes.clear();
            shared.set_tray(&mut model, tr::tr!("Awaiting sync checks..."), "loading");
            if !prompted_login {
                prompted_login = true;
                shared.dialog("login", tr::tr!("Log in"), "");
            }
            wait_command(&rx, &mut queue, Duration::from_millis(400));
            continue;
        }
        prompted_login = false;
        let pairs: Vec<(i32, PathBuf)> = match util::await_future(SyncDirsEntity::find().all(&db)) {
            Ok(dirs) => dirs
                .into_iter()
                .map(|dir| (dir.id, PathBuf::from(dir.local_path)))
                .collect(),
            Err(err) => {
                database_unavailable(shared, &mut model, &err);
                wait_command(&rx, &mut queue, Duration::from_secs(5));
                continue;
            }
        };
        watcher.sync_pairs(&pairs);

        for remote in remotes {
            pump(&rx, &mut queue);
            apply_deletes(&mut model, &db, &mut queue, shared);
            if shared.quit_requested() {
                break;
            }
            model.ensure_remote(&remote.name);
            let sync_dirs = match util::await_future(
                SyncDirsEntity::find()
                    .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                    .all(&db),
            ) {
                Ok(dirs) => dirs,
                Err(err) => {
                    database_unavailable(shared, &mut model, &err);
                    continue;
                }
            };
            for dir in &sync_dirs {
                model.ensure_dir(&remote.name, &dir.local_path, &dir.remote_path);
            }
            if !capabilities.contains_key(&remote.name) {
                match capabilities::probe(&db, &remote.name, &rclone_version) {
                    Ok(caps) => {
                        tracing::info!(remote = %remote.name, hash = ?caps.preferred_hash(), ?caps, "remote capabilities");
                        capabilities.insert(remote.name.clone(), caps);
                    }
                    Err(err) => {
                        tracing::warn!(remote = %remote.name, "could not read capabilities: {err}");
                        for dir in &sync_dirs {
                            if schedule.remote_checks.due(dir.id) {
                                pair_error(
                                    &mut model,
                                    shared,
                                    &remote.name,
                                    dir,
                                    Some(SyncError::General(
                                        format!("/{}", dir.remote_path),
                                        tr::tr!("The remote could not be reached [{}].", err),
                                    )),
                                );
                                schedule.remote_checks.done(dir.id);
                            }
                        }
                        continue;
                    }
                }
            }
            let caps = capabilities[&remote.name].clone();

            for sync_dir in sync_dirs {
                if shared.quit_requested() {
                    break;
                }
                pump(&rx, &mut queue);
                apply_deletes(&mut model, &db, &mut queue, shared);
                if !matches!(sync_dir.exists(&db), Ok(true)) {
                    continue;
                }
                sync_pair(
                    shared,
                    &db,
                    &mut model,
                    &rx,
                    &mut queue,
                    &remote,
                    &sync_dir,
                    &caps,
                    &mut watcher,
                    &mut schedule,
                );
            }
        }

        let error_count = model.error_dir_count();
        if error_count != 0 {
            model.tray_message = if error_count == 1 {
                tr::tr!("Finished sync checks with 1 error.")
            } else {
                tr::tr!("Finished sync checks with {} errors.", error_count)
            };
            model.tray_phase = "warning".into();
        } else {
            model.tray_message = tr::tr!("Finished sync checks.");
            model.tray_phase = "done".into();
        }
        shared.publish(&mut model);

        if shared.quit_requested() {
            break;
        }
        wait_command(&rx, &mut queue, Duration::from_millis(500));
    }
    Ok(())
}

/// Check one pair and run a pass when its local tree may have changed, its
/// remote check is due, or an item retry is due.
#[allow(clippy::too_many_arguments)]
fn sync_pair(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    caps: &Capabilities,
    watcher: &mut watch::LocalWatcher,
    schedule: &mut Schedule,
) {
    let id = sync_dir.id;
    let local_quiet = watcher.local_unchanged(id, Path::new(&sync_dir.local_path));
    if !local_quiet {
        schedule.local_trees.remove(&id);
    }
    let retry_due = schedule
        .retries
        .get(&id)
        .is_some_and(|at| *at <= store::now());
    if local_quiet && !schedule.remote_checks.due(id) && !retry_due {
        return;
    }
    if !pair_roots_available(db, remote, sync_dir, model, shared) {
        schedule.remote_checks.done(id);
        schedule.local_trees.remove(&id);
        return;
    }
    let names = (&sync_dir.local_path, &sync_dir.remote_path);
    shared.set_tray(model, tr::tr!("Syncing '{}'...", remote.name), "syncing");
    model.set_phase(&remote.name, names.0, names.1, "syncing");
    model.set_status(
        &remote.name,
        names.0,
        names.1,
        tr::tr!("Checking for changes..."),
    );
    shared.publish(model);

    let cached = if local_quiet {
        schedule.local_trees.remove(&id)
    } else {
        None
    };
    let full_scan = cached.is_none();
    let pair = Pair {
        db,
        remote,
        dir: sync_dir,
        caps,
    };
    let mut hooks = WorkerHooks {
        shared,
        db,
        model: &mut *model,
        rx,
        queue: &mut *queue,
        remote_name: &remote.name,
        sync_dir,
    };
    let result = pass::run(&pair, &mut hooks, cached);
    schedule.remote_checks.done(id);
    match result {
        Err(message) => {
            tracing::warn!(pair = %sync_dir.local_path, "pass stopped: {message}");
            pair_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                Some(SyncError::General(sync_dir.local_path.clone(), message)),
            );
        }
        Ok(report) if report.aborted => return,
        Ok(report) => {
            model.set_pair_error(&remote.name, names.0, names.1, None);
            model.set_issues(&remote.name, names.0, names.1, report.issues.clone());
            match report.next_retry {
                Some(at) => schedule.retries.insert(id, at),
                None => schedule.retries.remove(&id),
            };
            if report.settled() {
                // Only a full local observation restarts the full-scan clock.
                if full_scan {
                    watcher.mark_verified(id);
                }
                if let Some(tree) = report.local_tree {
                    schedule.local_trees.insert(id, tree);
                }
            }
        }
    }
    if matches!(sync_dir.exists(db), Ok(true)) {
        refresh_recovery(model, &pair);
        model.mark_finished(&remote.name, names.0, names.1);
        shared.publish(model);
    }
}

/// Show the pair's preserved versions so they can be restored.
fn refresh_recovery(model: &mut Model, pair: &Pair) {
    let items = match pass::recovery_list(pair) {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!("could not list preserved versions: {err}");
            return;
        }
    };
    let values = items
        .iter()
        .take(50)
        .map(|item| {
            json!({
                "id": item.id,
                "rel": item.rel,
                "side": item.side,
                "reason": item.reason,
                "size": item.size,
                "when": super::observe::Stamp::new(item.created_at, 0).to_string(),
            })
        })
        .collect();
    model.set_recovery(
        &pair.remote.name,
        &pair.dir.local_path,
        &pair.dir.remote_path,
        values,
    );
}

struct WorkerHooks<'a> {
    shared: &'a Shared,
    db: &'a DatabaseConnection,
    model: &'a mut Model,
    rx: &'a crossbeam_channel::Receiver<Command>,
    queue: &'a mut VecDeque<Command>,
    remote_name: &'a str,
    sync_dir: &'a SyncDirsModel,
}

impl pass::Hooks for WorkerHooks<'_> {
    fn checkpoint(&mut self) -> bool {
        pump(self.rx, self.queue);
        let mut kept = VecDeque::new();
        while let Some(cmd) = self.queue.pop_front() {
            match cmd {
                Command::DeleteRemote(name) => self.model.remote_deletions.push(name),
                Command::StopDir {
                    remote,
                    local,
                    remote_path,
                } => self.model.dir_deletions.push((remote, local, remote_path)),
                Command::SaveExclusions { local, text } => {
                    if let Err(err) = model::write_exclusions(&local, &text) {
                        self.shared
                            .dialog("error", tr::tr!("File/Folder Exclusions"), err);
                    }
                }
                Command::Quit => self.shared.quit.store(true, Ordering::Relaxed),
                other => kept.push_back(other),
            }
        }
        *self.queue = kept;
        process_deletions(self.model, self.db, self.shared);
        // Stop before any further write once the pair is gone or on quit.
        !self.shared.quit_requested() && matches!(self.sync_dir.exists(self.db), Ok(true))
    }

    fn progress(&mut self, text: String) {
        if self.model.set_status(
            self.remote_name,
            &self.sync_dir.local_path,
            &self.sync_dir.remote_path,
            text,
        ) {
            self.shared.publish(self.model);
        }
    }
}

fn pair_error(
    model: &mut Model,
    shared: &Shared,
    remote: &str,
    sync_dir: &SyncDirsModel,
    error: Option<SyncError>,
) {
    model.set_pair_error(remote, &sync_dir.local_path, &sync_dir.remote_path, error);
    if model.error_dir_count() != 0 {
        model.tray_phase = "warning".into();
    }
    shared.publish(model);
}

fn reload_model(db: &DatabaseConnection, model: &mut Model) -> Result<(), DbErr> {
    let remotes = util::await_future(RemotesEntity::find().all(db))?;
    for remote in remotes {
        model.ensure_remote(&remote.name);
        let dirs = util::await_future(
            SyncDirsEntity::find()
                .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                .all(db),
        )?;
        for dir in dirs {
            model.ensure_dir(&remote.name, &dir.local_path, &dir.remote_path);
        }
    }
    Ok(())
}

/// Copy the database aside before a schema change. A failed copy stops the
/// upgrade rather than migrating without a way back.
fn backup_before_migration(db: &DatabaseConnection, db_path: &Path) -> Result<(), String> {
    let pending = util::await_future(Migrator::get_pending_migrations(db))
        .map_err(|err| tr::tr!("Unable to run database migrations [{}]", err))?;
    let applied = util::await_future(Migrator::get_applied_migrations(db))
        .map_err(|err| tr::tr!("Unable to run database migrations [{}]", err))?;
    if pending.is_empty() || applied.is_empty() {
        return Ok(());
    }
    let backup = db_path.with_extension(format!("db.before-upgrade-{}", store::now()));
    fs::copy(db_path, &backup).map_err(|err| {
        tr::tr!(
            "Unable to back up the database before upgrading it [{}].",
            err
        )
    })?;
    tracing::info!(backup = %backup.display(), "database backed up before migration");
    Ok(())
}

fn database_unavailable(shared: &Shared, model: &mut Model, err: &DbErr) {
    tracing::error!("database read failed: {err}");
    shared.set_tray(
        model,
        tr::tr!("Synchronization paused: the database could not be read."),
        "warning",
    );
}

fn pump(rx: &crossbeam_channel::Receiver<Command>, queue: &mut VecDeque<Command>) {
    while let Ok(cmd) = rx.try_recv() {
        queue.push_back(cmd);
    }
}

fn wait_command(
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    dur: Duration,
) -> bool {
    match rx.recv_timeout(dur) {
        Ok(cmd) => {
            queue.push_back(cmd);
            pump(rx, queue);
            true
        }
        Err(_) => false,
    }
}

fn apply_deletes(
    model: &mut Model,
    db: &DatabaseConnection,
    queue: &mut VecDeque<Command>,
    shared: &Shared,
) {
    let mut kept = VecDeque::new();
    while let Some(cmd) = queue.pop_front() {
        match cmd {
            Command::DeleteRemote(name) => model.remote_deletions.push(name),
            Command::StopDir {
                remote,
                local,
                remote_path,
            } => model.dir_deletions.push((remote, local, remote_path)),
            Command::Quit => kept.push_back(Command::Quit),
            other => kept.push_back(other),
        }
    }
    *queue = kept;
    process_deletions(model, db, shared);
}

fn process_deletions(model: &mut Model, db: &DatabaseConnection, shared: &Shared) {
    while !model.dir_deletions.is_empty() {
        let (remote_name, local, remote_path) = model.dir_deletions.remove(0);
        let result: Result<(), DbErr> = util::await_future(async {
            let tx = db.begin().await?;
            if let Some(remote) = RemotesEntity::find()
                .filter(RemotesColumn::Name.eq(remote_name.as_str()))
                .one(&tx)
                .await?
            {
                if let Some(dir) = SyncDirsEntity::find()
                    .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                    .filter(SyncDirsColumn::LocalPath.eq(local.as_str()))
                    .filter(SyncDirsColumn::RemotePath.eq(remote_path.as_str()))
                    .one(&tx)
                    .await?
                {
                    forget_pair_state(&tx, dir.id).await?;
                    dir.delete(&tx).await?;
                }
            }
            tx.commit().await
        });
        match result {
            Ok(()) => {
                model.remove_dir(&remote_name, &local, &remote_path);
                note_preserved(db, shared, &remote_name, &local);
            }
            Err(err) => shared.dialog("error", "Could not stop synchronization", err.to_string()),
        }
    }
    while !model.remote_deletions.is_empty() {
        let remote_name = model.remote_deletions.remove(0);
        let result: Result<(), DbErr> = util::await_future(async {
            let tx = db.begin().await?;
            if let Some(remote) = RemotesEntity::find()
                .filter(RemotesColumn::Name.eq(remote_name.as_str()))
                .one(&tx)
                .await?
            {
                let dirs = SyncDirsEntity::find()
                    .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                    .all(&tx)
                    .await?;
                for dir in dirs {
                    forget_pair_state(&tx, dir.id).await?;
                    dir.delete(&tx).await?;
                }
                remote.delete(&tx).await?;
            }
            tx.commit().await
        });
        match result {
            Ok(()) => {
                model.remove_remote(&remote_name);
                if let Err(err) = rclone::sync::delete_config(&remote_name) {
                    shared.dialog("error", "Could not remove remote configuration", err.error);
                }
            }
            Err(err) => shared.dialog("error", "Could not remove remote", err.to_string()),
        }
    }
}

/// Stopping a pair keeps its preserved versions; say where they are.
fn note_preserved(db: &DatabaseConnection, shared: &Shared, remote: &str, local: &str) {
    let count = store::query(
        db,
        "SELECT COUNT(*) AS n FROM recovery_items WHERE remote_name = ? AND local_root = ? AND pruned_at IS NULL",
        vec![remote.into(), local.into()],
    )
    .ok()
    .and_then(|rows| rows.first().and_then(|row| row.try_get::<i64>("", "n").ok()))
    .unwrap_or(0);
    if count > 0 {
        shared.dialog(
            "info",
            tr::tr!("Preserved versions were kept"),
            tr::tr!(
                "{} preserved versions of this folder remain in its '.celestial-' folder and in '{}:{}'. They are not deleted automatically.",
                count,
                remote,
                super::recovery::REMOTE_RECOVERY_ROOT
            ),
        );
    }
}

async fn forget_pair_state(tx: &DatabaseTransaction, sync_dir_id: i32) -> Result<(), DbErr> {
    // Recovery records and incomplete operations are kept: they are keyed by
    // the pair's identity so preserved versions are never silently dropped.
    SyncItemsEntity::delete_many()
        .filter(SyncItemsColumn::SyncDirId.eq(sync_dir_id))
        .exec(tx)
        .await?;
    tx.execute(Statement::from_sql_and_values(
        sea_orm::DbBackend::Sqlite,
        "DELETE FROM item_states WHERE sync_dir_id = ?",
        vec![sync_dir_id.into()],
    ))
    .await?;
    Ok(())
}

fn find_pair(
    db: &DatabaseConnection,
    remote: &str,
    local: &str,
    remote_path: &str,
) -> Option<(RemotesModel, SyncDirsModel)> {
    let remote = util::await_future(
        RemotesEntity::find()
            .filter(RemotesColumn::Name.eq(remote))
            .one(db),
    )
    .ok()
    .flatten()?;
    let dir = util::await_future(
        SyncDirsEntity::find()
            .filter(SyncDirsColumn::RemoteId.eq(remote.id))
            .filter(SyncDirsColumn::LocalPath.eq(local))
            .filter(SyncDirsColumn::RemotePath.eq(remote_path))
            .one(db),
    )
    .ok()
    .flatten()?;
    Some((remote, dir))
}

/// The capabilities of a remote, probing it when they are not known yet.
fn caps_for(
    db: &DatabaseConnection,
    capabilities: &mut HashMap<String, Capabilities>,
    remote: &str,
) -> Result<Capabilities, String> {
    if let Some(caps) = capabilities.get(remote) {
        return Ok(caps.clone());
    }
    let caps = match capabilities::probe(db, remote, "unknown") {
        Ok(caps) => caps,
        // Offline: the capabilities recorded earlier still describe it.
        Err(err) => store::load_capabilities(db, remote)
            .map_err(|db_err| db_err.to_string())?
            .ok_or(err)?,
    };
    capabilities.insert(remote.to_owned(), caps.clone());
    Ok(caps)
}

fn apply_between_passes(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    queue: &mut VecDeque<Command>,
    schedule: &mut Schedule,
    capabilities: &mut HashMap<String, Capabilities>,
) {
    while let Some(cmd) = queue.pop_front() {
        if shared.quit_requested() {
            break;
        }
        match cmd {
            Command::Quit => shared.quit.store(true, Ordering::Relaxed),
            Command::DeleteRemote(name) => {
                model.remote_deletions.push(name);
                process_deletions(model, db, shared);
            }
            Command::StopDir {
                remote,
                local,
                remote_path,
            } => {
                model.dir_deletions.push((remote, local, remote_path));
                process_deletions(model, db, shared);
            }
            Command::SaveExclusions { local, text } => {
                if let Err(err) = model::write_exclusions(&local, &text) {
                    shared.dialog("error", tr::tr!("File/Folder Exclusions"), err);
                }
            }
            Command::ListDirs { remote, path } => {
                let names = list_dir_names(&remote, &path);
                let _ = shared.notices.send(super::Notice::Suggestions(
                    serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
                ));
            }
            Command::AddDir {
                remote,
                local,
                remote_path,
            } => add_dir(shared, db, model, &remote, &local, &remote_path),
            Command::SubmitLogin(req) => submit_login(shared, db, model, req),
            Command::DismissError {
                remote,
                local,
                remote_path,
                id,
            } => dismiss_error(db, model, schedule, &remote, &local, &remote_path, id),
            Command::ActivateError {
                remote,
                local,
                remote_path,
                id,
            } => activate_error(
                shared,
                db,
                model,
                capabilities,
                &remote,
                &local,
                &remote_path,
                id,
            ),
            Command::ConfirmDeletion {
                remote,
                local,
                remote_path,
                id,
                token,
            } => {
                let result = find_pair(db, &remote, &local, &remote_path)
                    .ok_or_else(|| tr::tr!("The synced folder no longer exists."))
                    .and_then(|(remote, dir)| {
                        let caps = caps_for(db, capabilities, &remote.name)?;
                        let id = i64::try_from(id).map_err(|err| err.to_string())?;
                        let record = store::conflict_by_id(db, dir.id, id)
                            .map_err(|err| err.to_string())?
                            .ok_or_else(|| tr::tr!("This item was already resolved."))?;
                        let pair = Pair {
                            db,
                            remote: &remote,
                            dir: &dir,
                            caps: &caps,
                        };
                        pass::delete_everywhere(&pair, &record.rel, &token)?;
                        store::acknowledge_conflict(db, dir.id, id)
                            .map_err(|err| err.to_string())?;
                        schedule.wake(dir.id);
                        refresh_recovery(model, &pair);
                        Ok(record.rel)
                    });
                match result {
                    Ok(rel) => shared.dialog(
                        "info",
                        tr::tr!("Deleted"),
                        tr::tr!(
                            "'{}' was deleted on both sides. Both versions were preserved and can be restored.",
                            rel
                        ),
                    ),
                    Err(err) => shared.dialog("error", tr::tr!("Nothing was deleted"), err),
                }
            }
            Command::Restore {
                remote,
                local,
                remote_path,
                recovery_id,
            } => {
                let result = find_pair(db, &remote, &local, &remote_path)
                    .ok_or_else(|| tr::tr!("The synced folder no longer exists."))
                    .and_then(|(remote, dir)| {
                        let caps = caps_for(db, capabilities, &remote.name)?;
                        let pair = Pair {
                            db,
                            remote: &remote,
                            dir: &dir,
                            caps: &caps,
                        };
                        let rel = pass::restore(&pair, recovery_id)?;
                        schedule.wake(dir.id);
                        refresh_recovery(model, &pair);
                        Ok(rel)
                    });
                match result {
                    Ok(rel) => shared.dialog(
                        "info",
                        tr::tr!("Restored"),
                        tr::tr!(
                            "'{}' was restored; the version it replaced was preserved. The restored version is synchronized next.",
                            rel
                        ),
                    ),
                    Err(err) => shared.dialog("error", tr::tr!("Nothing was restored"), err),
                }
            }
        }
        shared.publish(model);
    }
}

/// Dismissing an item retries it on the next loop; dismissing a pair problem
/// checks the pair again.
fn dismiss_error(
    db: &DatabaseConnection,
    model: &mut Model,
    schedule: &mut Schedule,
    remote: &str,
    local: &str,
    remote_path: &str,
    id: u64,
) {
    let Some(dismissed) = model.dismiss_error(remote, local, remote_path, id) else {
        return;
    };
    let Some((_, sync_dir)) = find_pair(db, remote, local, remote_path) else {
        return;
    };
    let result = match &dismissed.error {
        SyncError::Item { rel, state, .. } if state == store::RETRYING => {
            store::retry_now(db, sync_dir.id, rel)
        }
        SyncError::Item { rel, .. } => store::clear_item_state(db, sync_dir.id, rel),
        SyncError::Conflict { id, .. } => {
            store::acknowledge_conflict(db, sync_dir.id, *id).map(|_| ())
        }
        SyncError::General(..) => Ok(()),
    };
    if let Err(err) = result {
        tracing::warn!("could not reschedule a dismissed item: {err}");
    }
    schedule.wake(sync_dir.id);
}

#[allow(clippy::too_many_arguments)]
fn activate_error(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    capabilities: &mut HashMap<String, Capabilities>,
    remote_name: &str,
    local: &str,
    remote_path: &str,
    id: u64,
) {
    let Some(stored) = model.error_by_id(remote_name, local, remote_path, id) else {
        return;
    };
    if let SyncError::Conflict {
        id: conflict_id,
        rel,
        kind,
        detail,
    } = &stored.error
    {
        // Bind a deletion to exactly what is shown now.
        let token = (kind == "edit_delete")
            .then(|| {
                let (remote, dir) = find_pair(db, remote_name, local, remote_path)?;
                let caps = caps_for(db, capabilities, &remote.name).ok()?;
                let pair = Pair {
                    db,
                    remote: &remote,
                    dir: &dir,
                    caps: &caps,
                };
                pass::item_token(&pair, rel).ok()
            })
            .flatten();
        shared.dialog(
            "conflict-info",
            model::conflict_title(kind, rel),
            json!({
                "remote": remote_name,
                "local": local,
                "remotePath": remote_path,
                "id": id,
                "conflictId": conflict_id,
                "text": model::conflict_summary(kind, detail),
                "canDelete": token.is_some(),
                "token": token.unwrap_or_default(),
            })
            .to_string(),
        );
        return;
    }
    let title = match &stored.error {
        SyncError::General(..) => tr::tr!("Would you like to dismiss this error?"),
        SyncError::Item { state, reason, .. } if state == store::RETRYING => tr::tr!(
            "{}\n\nThis item is retried automatically. Retry it now?",
            reason
        ),
        SyncError::Item { reason, .. } => tr::tr!(
            "{}\n\nNothing was changed. Check this item again now?",
            reason
        ),
        SyncError::Conflict { .. } => return,
    };
    shared.dialog(
        "confirm-dismiss",
        title,
        error_ref(remote_name, local, remote_path, id),
    );
}

fn error_ref(remote: &str, local: &str, remote_path: &str, id: u64) -> String {
    json!({
        "remote": remote,
        "local": local,
        "remotePath": remote_path,
        "id": id,
    })
    .to_string()
}

fn submit_login(shared: &Shared, db: &DatabaseConnection, model: &mut Model, req: LoginRequest) {
    shared.cancel_auth.store(false, Ordering::Relaxed);
    let label = req.kind_label();
    let result = login::create_remote(db, &req, &shared.cancel_auth, || {
        shared.dialog(
            "auth",
            tr::tr!("Authenticating to {}...", label),
            tr::tr!(
                "Follow the link that opened in your browser, and come back once you've finished."
            ),
        );
    });
    match result {
        Ok(remote) => {
            model.ensure_remote(&remote.name);
            shared.dialog("login-ok", "", remote.name);
        }
        Err(err) if err == "cancelled" => shared.dialog("auth-done", "", ""),
        Err(err) => {
            shared.dialog("auth-done", "", "");
            shared.dialog("error", tr::tr!("Unable to log in"), err);
        }
    }
}

fn add_dir(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    remote_name: &str,
    local_raw: &str,
    remote_raw: &str,
) {
    if !Path::new(local_raw).is_absolute() {
        shared.dialog(
            "error",
            tr::tr!("The specified local directory needs to be an absolute path"),
            "",
        );
        return;
    }
    let Some(db_remote) = util::await_future(
        RemotesEntity::find()
            .filter(RemotesColumn::Name.eq(remote_name))
            .one(db),
    )
    .ok()
    .flatten() else {
        shared.dialog("error", tr::tr!("Unable to log in"), remote_name.to_owned());
        return;
    };
    let local_text = format!("/{}", util::strip_slashes(local_raw));
    let remote_text = util::strip_slashes(remote_raw);
    let local_path = Path::new(&local_text);
    match rclone::sync::stat(remote_name, &remote_text) {
        Ok(None) => {
            shared.dialog(
                "error",
                tr::tr!("The specified remote directory doesn't exist"),
                "",
            );
            return;
        }
        Err(err) => {
            shared.dialog(
                "error",
                tr::tr!("Failed to check if the specified remote directory exists"),
                err.error,
            );
            return;
        }
        Ok(Some(item)) if !item.is_dir => {
            shared.dialog("error", "The specified remote path is not a directory", "");
            return;
        }
        Ok(Some(_)) => {}
    }
    let existing = util::await_future(
        SyncDirsEntity::find()
            .filter(SyncDirsColumn::RemoteId.eq(db_remote.id))
            .filter(SyncDirsColumn::LocalPath.eq(local_text.clone()))
            .filter(SyncDirsColumn::RemotePath.eq(remote_text.clone()))
            .one(db),
    );
    if existing.ok().flatten().is_some() {
        shared.dialog(
            "error",
            tr::tr!("The specified directory pair is already being synced"),
            "",
        );
        return;
    }
    if !local_path.exists() {
        shared.dialog(
            "error",
            tr::tr!("The specified local directory doesn't exist"),
            "",
        );
        return;
    }
    if !local_path.is_dir() {
        shared.dialog(
            "error",
            tr::tr!("The specified local path isn't a directory"),
            "",
        );
        return;
    }
    if !local_path.is_absolute() {
        shared.dialog(
            "error",
            tr::tr!("The specified local directory needs to be an absolute path"),
            "",
        );
        return;
    }
    if util::await_future(
        SyncDirsActiveModel {
            remote_id: ActiveValue::Set(db_remote.id),
            local_path: ActiveValue::Set(local_text.clone()),
            remote_path: ActiveValue::Set(remote_text.clone()),
            ..Default::default()
        }
        .insert(db),
    )
    .is_err()
    {
        shared.dialog(
            "error",
            tr::tr!("Unable to connect to database [{}].", "insert"),
            "",
        );
        return;
    }
    model.ensure_dir(remote_name, &local_text, &remote_text);
}

fn list_dir_names(remote: &str, path: &str) -> Vec<String> {
    match rclone::sync::list(remote, path, false, RcloneListFilter::Dirs) {
        Ok(items) => items.into_iter().map(|item| item.name).collect(),
        Err(_) => Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn pair_roots_available(
    db: &DatabaseConnection,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    model: &mut Model,
    shared: &Shared,
) -> bool {
    if !matches!(sync_dir.exists(db), Ok(true)) {
        return false;
    }
    if let Err(err) = fs::read_dir(&sync_dir.local_path) {
        pair_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            Some(SyncError::General(
                sync_dir.local_path.clone(),
                format!("Local sync folder is unavailable: {err}"),
            )),
        );
        return false;
    }
    match rclone::sync::stat(&remote.name, &sync_dir.remote_path) {
        Ok(Some(item)) if item.is_dir => true,
        result => {
            let message = match result {
                Err(err) => err.error,
                _ => "Remote sync folder is unavailable; automatic deletion is paused".into(),
            };
            pair_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                Some(SyncError::General(sync_dir.remote_path.clone(), message)),
            );
            false
        }
    }
}

/// How often a pair with no local changes lists its remote folder. Local
/// changes are still picked up on the next pass.
const REMOTE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// When each pair's remote folder was last listed successfully.
#[derive(Default)]
struct RemoteChecks(HashMap<i32, Instant>);

impl RemoteChecks {
    fn due(&self, id: i32) -> bool {
        self.0
            .get(&id)
            .is_none_or(|at| at.elapsed() >= REMOTE_CHECK_INTERVAL)
    }

    fn done(&mut self, id: i32) {
        self.0.insert(id, Instant::now());
    }

    fn forget(&mut self, id: i32) {
        self.0.remove(&id);
    }
}

trait KindLabel {
    fn kind_label(&self) -> String;
}

impl KindLabel for LoginRequest {
    fn kind_label(&self) -> String {
        match self.kind.as_str() {
            "dropbox" => "Dropbox",
            "gdrive" => "Google Drive",
            "pcloud" => "pCloud",
            "nextcloud" => "Nextcloud",
            "owncloud" => "Owncloud",
            "protondrive" => "Proton Drive",
            "webdav" => "WebDAV",
            other => other,
        }
        .to_owned()
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
