//! The sync loop from the GTK app, with widget updates replaced by [`Model`].
//!
//! Decisions about which copy wins are unchanged. Dialogs are reported to the
//! window and applied on a later pass, at the same points the old loop was
//! free to take a click.

use super::model::{self, Model, SyncError, FILE_IGNORE_NAME};
use super::{Command, Shared};
use crate::{
    entities::{
        RemotesColumn, RemotesEntity, RemotesModel, SyncDirsActiveModel, SyncDirsColumn,
        SyncDirsEntity, SyncDirsModel, SyncItemsActiveModel, SyncItemsColumn, SyncItemsEntity,
    },
    login::{self, LoginRequest},
    migrations::{Migrator, MigratorTrait},
    rclone::{self, RcloneListFilter},
    util,
};
use sea_orm::{entity::prelude::*, ActiveValue, Database, DatabaseConnection};
use serde_json::json;
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    thread,
    time::{Duration, SystemTime},
};

pub fn spawn(
    shared: Shared,
    rx: crossbeam_channel::Receiver<Command>,
) -> std::thread::JoinHandle<()> {
    thread::Builder::new()
        .name("celeste-sync".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&shared, rx)));
            if let Err(payload) = result {
                let message = panic_message(payload);
                tracing::error!("sync worker panicked: {message}");
                shared.dialog(
                    "fatal",
                    tr::tr!("Unknown Error"),
                    format!(
                        "{}\n\n{message}",
                        tr::tr!("An unknown error has occurred while running. This is an internal issue with Celeste and should be reported.\n\nThe following backtrace may help with debugging the issue - note that it may contain information such as login tokens/keys, so avoid posting the information publicly:")
                    ),
                );
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

fn worker(shared: &Shared, rx: crossbeam_channel::Receiver<Command>) -> Result<(), String> {
    let mut config = util::get_config_dir();
    fs::create_dir_all(&config).map_err(|err| {
        tr::tr!("Unable to create Celeste's config directory [{}].", err)
    })?;
    config.push("rclone.conf");
    librclone::initialize();
    librclone::rpc("config/setpath", json!({ "path": config.display().to_string() }).to_string())
        .map_err(|err| format!("Could not set the rclone config path: {err}"))?;

    let mut db_path = util::get_config_dir();
    db_path.push("celeste.db");
    if !db_path.exists() {
        File::create(&db_path).map_err(|err| {
            tr::tr!("Unable to create Celeste's database file [{}].", err)
        })?;
    }
    let db = util::await_future(Database::connect(format!("sqlite://{}", db_path.display())))
        .map_err(|err| tr::tr!("Unable to connect to database [{}].", err))?;
    util::await_future(Migrator::up(&db, None))
        .map_err(|err| tr::tr!("Unable to run database migrations [{}]", err))?;

    let mut model = Model::new();
    reload_model(&db, &mut model);
    shared.publish(&mut model);

    let mut queue = VecDeque::new();
    let mut prompted_login = false;
    let please_resolve = format!(" {}", tr::tr!("Please resolve the reported syncing issues."));

    loop {
        pump(&rx, &mut queue);
        if shared.quit_requested() {
            break;
        }
        apply_deletes(&mut model, &db, &mut queue);
        shared.publish(&mut model);
        apply_between_passes(shared, &db, &mut model, &mut queue, &please_resolve);
        if shared.quit_requested() {
            break;
        }

        let remotes = util::await_future(RemotesEntity::find().all(&db)).unwrap_or_default();
        if remotes.is_empty() {
            model.remotes.clear();
            shared.set_tray(
                &mut model,
                tr::tr!("Awaiting sync checks..."),
                "loading",
            );
            if !prompted_login {
                prompted_login = true;
                shared.dialog("login", tr::tr!("Log in"), "");
            }
            if wait_command(&rx, &mut queue, Duration::from_millis(400)) {
                continue;
            }
            continue;
        }
        prompted_login = false;

        for remote in remotes {
            pump(&rx, &mut queue);
            apply_deletes(&mut model, &db, &mut queue);
            if shared.quit_requested() {
                break;
            }
            model.ensure_remote(&remote.name);
            let sync_dirs = util::await_future(
                SyncDirsEntity::find()
                    .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                    .all(&db),
            )
            .unwrap_or_default();
            for dir in &sync_dirs {
                model.ensure_dir(&remote.name, &dir.local_path, &dir.remote_path);
            }

            for sync_dir in sync_dirs {
                if shared.quit_requested() {
                    break;
                }
                pump(&rx, &mut queue);
                apply_deletes(&mut model, &db, &mut queue);
                if !sync_dir.exists(&db) {
                    continue;
                }
                let should_sync = directory_changed(&db, &remote, &sync_dir);
                if !should_sync {
                    model.set_status(
                        &remote.name,
                        &sync_dir.local_path,
                        &sync_dir.remote_path,
                        tr::tr!("Files are synced."),
                    );
                    model.set_phase(
                        &remote.name,
                        &sync_dir.local_path,
                        &sync_dir.remote_path,
                        "done",
                    );
                    shared.publish(&mut model);
                    continue;
                }

                shared.set_tray(
                    &mut model,
                    tr::tr!("Syncing '{}'...", remote.name),
                    "syncing",
                );
                if model.has_errors(&remote.name, &sync_dir.local_path, &sync_dir.remote_path) {
                    continue;
                }
                model.set_phase(&remote.name, &sync_dir.local_path, &sync_dir.remote_path, "syncing");
                model.set_status(
                    &remote.name,
                    &sync_dir.local_path,
                    &sync_dir.remote_path,
                    tr::tr!("Checking for changes..."),
                );
                shared.publish(&mut model);

                let synced: RefCell<Vec<(String, String)>> = RefCell::new(Vec::new());
                sync_local_directory(
                    Path::new(&sync_dir.local_path),
                    &remote,
                    &sync_dir,
                    &db,
                    &mut model,
                    shared,
                    &synced,
                    &rx,
                    &mut queue,
                    &please_resolve,
                );
                if sync_dir.exists(&db) && !shared.quit_requested() {
                    sync_remote_directory(
                        &sync_dir.remote_path,
                        &remote,
                        &sync_dir,
                        &db,
                        &mut model,
                        shared,
                        &synced,
                        &rx,
                        &mut queue,
                        &please_resolve,
                    );
                }
                if shared.quit_requested() {
                    break;
                }
                if !sync_dir.exists(&db) {
                    continue;
                }
                model.mark_finished(
                    &remote.name,
                    &sync_dir.local_path,
                    &sync_dir.remote_path,
                    &please_resolve,
                );
                shared.publish(&mut model);
            }
        }

        let error_count = model.error_dir_count();
        if error_count != 0 {
            let error_msg = if error_count == 1 {
                "Finished sync checks with 1 error.".to_string()
            } else {
                tr::tr!("Finished sync checks with {} errors.", error_count)
            };
            model.tray_message = error_msg;
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

fn reload_model(db: &DatabaseConnection, model: &mut Model) {
    let remotes = util::await_future(RemotesEntity::find().all(db)).unwrap_or_default();
    for remote in remotes {
        model.ensure_remote(&remote.name);
        let dirs = util::await_future(
            SyncDirsEntity::find()
                .filter(SyncDirsColumn::RemoteId.eq(remote.id))
                .all(db),
        )
        .unwrap_or_default();
        for dir in dirs {
            model.ensure_dir(&remote.name, &dir.local_path, &dir.remote_path);
        }
    }
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

fn apply_deletes(model: &mut Model, db: &DatabaseConnection, queue: &mut VecDeque<Command>) {
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
    process_deletions(model, db);
}

fn process_deletions(model: &mut Model, db: &DatabaseConnection) {
    while !model.dir_deletions.is_empty() {
        let (remote_name, local, remote_path) = model.dir_deletions.remove(0);
        model.remove_dir(&remote_name, &local, &remote_path);
        util::await_future(async {
            let Some(sync_dir) = SyncDirsEntity::find()
                .filter(SyncDirsColumn::LocalPath.eq(local.clone()))
                .filter(SyncDirsColumn::RemotePath.eq(remote_path.clone()))
                .one(db)
                .await
                .ok()
                .flatten()
            else {
                return;
            };
            let _ = SyncItemsEntity::delete_many()
                .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                .exec(db)
                .await;
            let _ = sync_dir.delete(db).await;
        });
    }

    while !model.remote_deletions.is_empty() {
        let remote_name = model.remote_deletions.remove(0);
        model.remove_remote(&remote_name);
        util::await_future(async {
            let Some(db_remote) = RemotesEntity::find()
                .filter(RemotesColumn::Name.eq(remote_name.clone()))
                .one(db)
                .await
                .ok()
                .flatten()
            else {
                return;
            };
            let sync_dirs = SyncDirsEntity::find()
                .filter(SyncDirsColumn::RemoteId.eq(db_remote.id))
                .all(db)
                .await
                .unwrap_or_default();
            for sync_dir in sync_dirs {
                let _ = SyncItemsEntity::delete_many()
                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                    .exec(db)
                    .await;
                let _ = sync_dir.delete(db).await;
            }
            let _ = db_remote.delete(db).await;
        });
        let _ = rclone::sync::delete_config(&remote_name);
    }
}

fn apply_between_passes(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) {
    while let Some(cmd) = queue.pop_front() {
        if shared.quit_requested() {
            break;
        }
        match cmd {
            Command::Quit => shared.quit.store(true, Ordering::Relaxed),
            Command::DeleteRemote(name) => {
                model.remote_deletions.push(name);
                process_deletions(model, db);
            }
            Command::StopDir {
                remote,
                local,
                remote_path,
            } => {
                model.dir_deletions.push((remote, local, remote_path));
                process_deletions(model, db);
            }
            Command::SaveExclusions { local, text } => {
                if let Err(err) = model::write_exclusions(&local, &text) {
                    shared.dialog("error", tr::tr!("File/Folder Exclusions"), err);
                }
            }
            Command::ListDirs { remote, path } => {
                let names = list_dir_names(&remote, &path);
                let _ = shared
                    .notices
                    .send(super::Notice::Suggestions(serde_json::to_string(&names).unwrap_or_else(|_| "[]".into())));
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
            } => {
                model.dismiss_error(&remote, &local, &remote_path, id, please_resolve);
            }
            Command::ActivateError {
                remote,
                local,
                remote_path,
                id,
            } => activate_error(shared, db, model, &remote, &local, &remote_path, id, please_resolve),
            Command::ChooseConflict {
                remote,
                local,
                remote_path,
                id,
                keep_local,
            } => choose_conflict(
                shared,
                db,
                model,
                &remote,
                &local,
                &remote_path,
                id,
                keep_local,
                please_resolve,
            ),
        }
        shared.publish(model);
    }
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
            tr::tr!("Follow the link that opened in your browser, and come back once you've finished."),
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
        Ok(Some(_)) => {}
    }
    let existing = util::await_future(
        SyncDirsEntity::find()
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
fn activate_error(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    remote_name: &str,
    local: &str,
    remote_path: &str,
    id: u64,
    please_resolve: &str,
) {
    let Some(error) = model.error_by_id(remote_name, local, remote_path, id) else {
        return;
    };
    match error {
        SyncError::General(_, _) => {
            shared.dialog(
                "confirm-dismiss",
                tr::tr!("Would you like to dismiss this error?"),
                error_ref(remote_name, local, remote_path, id),
            );
        }
        SyncError::BothMoreCurrent(local_item, remote_item) => {
            resolve_conflict_probe(
                shared,
                db,
                model,
                remote_name,
                local,
                remote_path,
                id,
                &local_item,
                &remote_item,
                please_resolve,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn choose_conflict(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    remote_name: &str,
    local: &str,
    remote_path: &str,
    id: u64,
    keep_local: bool,
    please_resolve: &str,
) {
    let Some(SyncError::BothMoreCurrent(local_item, remote_item)) =
        model.error_by_id(remote_name, local, remote_path, id)
    else {
        return;
    };
    let copied = if keep_local {
        copy_local_to_remote(shared, remote_name, &local_item, &remote_item)
    } else {
        copy_remote_to_local(shared, remote_name, &local_item, &remote_item)
    };
    if copied {
        let _ = update_item_timestamps(db, remote_name, &local_item, &remote_item);
        model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_conflict_probe(
    shared: &Shared,
    db: &DatabaseConnection,
    model: &mut Model,
    remote_name: &str,
    local: &str,
    remote_path: &str,
    id: u64,
    local_item: &str,
    remote_item: &str,
    please_resolve: &str,
) {
    let local_path = Path::new(local_item);
    let remote_stat = match rclone::sync::stat(remote_name, remote_item) {
        Ok(item) => item,
        Err(err) => {
            shared.dialog(
                "error",
                tr::tr!("Unable to fetch data for '{}' from the remote.", remote_item),
                err.error,
            );
            return;
        }
    };
    if !local_path.exists() && remote_stat.is_none() {
        shared.dialog(
            "info",
            tr::tr!("File Update"),
            tr::tr!("Neither the local item or remote item exists anymore. This error will now be removed."),
        );
        model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
        return;
    }
    if local_path.exists() && remote_stat.is_none() {
        shared.dialog(
            "info",
            tr::tr!("File Update"),
            tr::tr!("Only the local item exists now, so it will be synced to the remote."),
        );
        if copy_local_to_remote(shared, remote_name, local_item, remote_item) {
            let _ = update_item_timestamps(db, remote_name, local_item, remote_item);
            model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
        }
        return;
    }
    if !local_path.exists() && remote_stat.is_some() {
        shared.dialog(
            "info",
            tr::tr!("File Update"),
            tr::tr!("Only the remote item exists now, so it will be synced to the local machine."),
        );
        if copy_remote_to_local(shared, remote_name, local_item, remote_item) {
            let _ = update_item_timestamps(db, remote_name, local_item, remote_item);
            model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
        }
        return;
    }
    shared.dialog(
        "conflict",
        tr::tr!(
            "Both the local item '{}' and remote item '{}' have been updated since the last sync.",
            util::fmt_home(local_item),
            remote_item
        ),
        error_ref(remote_name, local, remote_path, id),
    );
}

fn copy_local_to_remote(shared: &Shared, remote_name: &str, local_item: &str, remote_item: &str) -> bool {
    if let Err(err) = rclone::sync::copy_to_remote(local_item, remote_name, remote_item) {
        shared.dialog(
            "error",
            tr::tr!(
                "Failed to sync '{}' to '{}' on remote.",
                util::fmt_home(local_item),
                remote_item
            ),
            err.error,
        );
        false
    } else {
        true
    }
}

fn copy_remote_to_local(shared: &Shared, remote_name: &str, local_item: &str, remote_item: &str) -> bool {
    if let Err(err) = rclone::sync::copy_to_local(local_item, remote_name, remote_item) {
        shared.dialog(
            "error",
            tr::tr!(
                "Failed to sync '{}' on remote to '{}'.",
                remote_item,
                util::fmt_home(local_item)
            ),
            err.error,
        );
        false
    } else {
        true
    }
}

fn update_item_timestamps(
    db: &DatabaseConnection,
    remote_name: &str,
    local_item: &str,
    remote_item: &str,
) -> Result<(), ()> {
    let local_timestamp = Path::new(local_item)
        .metadata()
        .map_err(|_| ())?
        .modified()
        .map_err(|_| ())?
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| ())?
        .as_secs();
    let remote_timestamp = rclone::sync::stat(remote_name, remote_item)
        .map_err(|_| ())?
        .ok_or(())?
        .mod_time
        .unix_timestamp();
    let model = util::await_future(
        SyncItemsEntity::find()
            .filter(SyncItemsColumn::LocalPath.eq(local_item))
            .filter(SyncItemsColumn::RemotePath.eq(remote_item))
            .one(db),
    )
    .map_err(|_| ())?
    .ok_or(())?;
    let mut active: SyncItemsActiveModel = model.into();
    active.last_local_timestamp = ActiveValue::Set(local_timestamp.try_into().map_err(|_| ())?);
    active.last_remote_timestamp = ActiveValue::Set(remote_timestamp.try_into().map_err(|_| ())?);
    util::await_future(active.update(db)).map_err(|_| ())?;
    Ok(())
}

fn directory_changed(db: &DatabaseConnection, remote: &RemotesModel, sync_dir: &SyncDirsModel) -> bool {
    let local_glob = format!("{}/**/*", sync_dir.local_path);
    if let Ok(paths) = glob::glob(&local_glob) {
        for maybe_path in paths {
            let Ok(path) = maybe_path else {
                return true;
            };
            let Ok(file) = File::open(&path) else {
                return true;
            };
            let Ok(metadata) = file.metadata() else {
                return true;
            };
            let Ok(modified) = metadata.modified() else {
                return true;
            };
            let Ok(current_timestamp) = modified.duration_since(SystemTime::UNIX_EPOCH) else {
                return true;
            };
            let current_timestamp = current_timestamp.as_secs();
            let maybe_db_sync_item = util::await_future(
                SyncItemsEntity::find()
                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                    .filter(SyncItemsColumn::LocalPath.eq(path.display().to_string()))
                    .one(db),
            )
            .ok()
            .flatten();
            let Some(db_sync_item) = maybe_db_sync_item else {
                return true;
            };
            if current_timestamp != u64::try_from(db_sync_item.last_local_timestamp).unwrap_or(0) {
                return true;
            }
        }
    } else {
        return true;
    }

    if let Ok(paths) = rclone::sync::list(&remote.name, &sync_dir.remote_path, true, RcloneListFilter::All)
    {
        for path in paths {
            let stripped_path = if path.name.contains('/') {
                path.name
                    .strip_suffix(&format!("{}/", sync_dir.remote_path))
                    .unwrap_or(path.name.as_str())
                    .to_string()
            } else {
                path.name.clone()
            };
            let maybe_db_sync_item = util::await_future(
                SyncItemsEntity::find()
                    .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                    .filter(SyncItemsColumn::RemotePath.eq(stripped_path))
                    .one(db),
            )
            .ok()
            .flatten();
            let Some(db_sync_item) = maybe_db_sync_item else {
                return true;
            };
            if path.mod_time.unix_timestamp() != i64::from(db_sync_item.last_remote_timestamp) {
                return true;
            }
        }
    } else {
        return true;
    }

    let sync_items = util::await_future(
        SyncItemsEntity::find()
            .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
            .all(db),
    )
    .unwrap_or_default();
    for sync_item in sync_items {
        let remote_path = if !sync_dir.remote_path.is_empty() {
            format!("{}/{}", sync_dir.remote_path, sync_item.remote_path)
        } else {
            sync_item.remote_path.clone()
        };
        let maybe_remote_timestamp = rclone::sync::stat(&remote.name, &remote_path)
            .ok()
            .flatten()
            .map(|remote_item| remote_item.mod_time.unix_timestamp());
        if !Path::new(&sync_item.local_path).exists() && maybe_remote_timestamp.is_none() {
            util::await_future(async {
                if let Some(row) = SyncItemsEntity::find()
                    .filter(SyncItemsColumn::Id.eq(sync_item.id))
                    .one(db)
                    .await
                    .ok()
                    .flatten()
                {
                    let _ = row.delete(db).await;
                }
            });
            continue;
        }
        let Some(remote_timestamp) = maybe_remote_timestamp else {
            return true;
        };
        let local_timestamp = match fs::metadata(&sync_item.local_path) {
            Ok(metadata) => metadata
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            Err(_) => None,
        };
        let Some(local_timestamp) = local_timestamp else {
            return true;
        };
        if i64::try_from(local_timestamp).ok() != Some(i64::from(sync_item.last_local_timestamp))
            || remote_timestamp != i64::from(sync_item.last_remote_timestamp)
        {
            return true;
        }
    }
    false
}

fn load_ignore_globs(ignore_file: &Path) -> Vec<glob::Pattern> {
    if !ignore_file.is_file() {
        return Vec::new();
    }
    let Ok(_lock) = file_lock::FileLock::lock(
        ignore_file,
        true,
        file_lock::FileOptions::new().read(true).write(true),
    ) else {
        return Vec::new();
    };
    let Ok(content) = fs::read_to_string(ignore_file) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| glob::Pattern::new(line).ok())
        .collect()
}

fn note_progress(
    model: &mut Model,
    shared: &Shared,
    remote: &str,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    status: String,
) {
    if !sync_dir.exists(db) {
        return;
    }
    if model.set_status(remote, &sync_dir.local_path, &sync_dir.remote_path, status) {
        shared.publish(model);
    }
}

fn record_error(
    model: &mut Model,
    shared: &Shared,
    remote: &str,
    sync_dir: &SyncDirsModel,
    error: SyncError,
) {
    if model.error_dir_count() == 0 {
        model.tray_phase = "syncing".into();
    }
    model.add_error(remote, &sync_dir.local_path, &sync_dir.remote_path, error);
    model.tray_phase = "warning".into();
    shared.publish(model);
}

struct Walk<'a> {
    remote: &'a RemotesModel,
    sync_dir: &'a SyncDirsModel,
    db: &'a DatabaseConnection,
    model: &'a mut Model,
    shared: &'a Shared,
    synced: &'a RefCell<Vec<(String, String)>>,
    rx: &'a crossbeam_channel::Receiver<Command>,
    queue: &'a mut VecDeque<Command>,
    please_resolve: &'a str,
}

fn checkpoint(walk: &mut Walk<'_>) {
    pump(walk.rx, walk.queue);
    let mut kept = VecDeque::new();
    while let Some(cmd) = walk.queue.pop_front() {
        match cmd {
            Command::DeleteRemote(name) => walk.model.remote_deletions.push(name),
            Command::StopDir {
                remote,
                local,
                remote_path,
            } => walk.model.dir_deletions.push((remote, local, remote_path)),
            Command::SaveExclusions { local, text } => {
                if let Err(err) = model::write_exclusions(&local, &text) {
                    walk.shared
                        .dialog("error", tr::tr!("File/Folder Exclusions"), err);
                }
            }
            Command::Quit => walk.shared.quit.store(true, Ordering::Relaxed),
            other => kept.push_back(other),
        }
    }
    *walk.queue = kept;
    process_deletions(walk.model, walk.db);
}

#[allow(clippy::too_many_arguments)]
fn sync_local_directory(
    local_dir: &Path,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<Vec<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) {
    let mut walk = Walk {
        remote,
        sync_dir,
        db,
        model,
        shared,
        synced,
        rx,
        queue,
        please_resolve,
    };
    checkpoint(&mut walk);
    let Walk {
        remote,
        sync_dir,
        db,
        model,
        shared,
        synced,
        rx,
        queue,
        please_resolve,
    } = walk;

    let dir_string = local_dir.to_str().unwrap_or("").to_owned();
    note_progress(
        model,
        shared,
        &remote.name,
        sync_dir,
        db,
        tr::tr!("Checking '{}' for changes...", util::fmt_home(&dir_string)),
    );
    let directory = match fs::read_dir(local_dir) {
        Ok(dir) => dir,
        Err(err) => {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(dir_string, err.to_string()),
            );
            return;
        }
    };
    let ignore_globs = load_ignore_globs(&PathBuf::from(format!(
        "{}/{}",
        sync_dir.local_path, FILE_IGNORE_NAME
    )));

    for item in directory {
        if shared.quit_requested() {
            break;
        }
        let mut walk = Walk {
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        };
        checkpoint(&mut walk);
        let Walk {
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        } = walk;
        if !sync_dir.exists(db) {
            break;
        }
        let item = match item {
            Ok(item) => item,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(dir_string.clone(), err.to_string()),
                );
                continue;
            }
        };
        let local_path = item.path().to_str().unwrap_or("").to_owned();
        let remote_path = {
            let Some(local_path_stripped) =
                local_path.strip_prefix(&format!("{}/", sync_dir.local_path))
            else {
                continue;
            };
            let stripped_path = local_path_stripped.strip_suffix('/').unwrap_or(local_path_stripped);
            if sync_dir.remote_path.is_empty() {
                stripped_path.to_owned()
            } else {
                format!("{}/{}", sync_dir.remote_path, stripped_path)
            }
        };
        let stripped_remote_path =
            if remote_path.contains('/') && sync_dir.remote_path.contains('/') {
                remote_path
                    .strip_prefix(&format!("{}/", sync_dir.remote_path))
                    .unwrap_or(&remote_path)
                    .to_owned()
            } else {
                remote_path.clone()
            };
        note_progress(
            model,
            shared,
            &remote.name,
            sync_dir,
            db,
            tr::tr!("Checking '{}' for changes...", util::fmt_home(&local_path)),
        );
        if ignore_globs.iter().any(|pattern| pattern.matches(&stripped_remote_path)) {
            continue;
        }
        synced.borrow_mut().push((local_path.clone(), remote_path.clone()));

        let get_local_file_timestamp = || {
            item.metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
        };
        let Some(local_utc_timestamp) = get_local_file_timestamp() else {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(local_path, "Could not read the local modification time".into()),
            );
            continue;
        };
        let remote_item = match rclone::sync::stat(&remote.name, &remote_path) {
            Ok(item) => item,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(remote_path, err.error),
                );
                continue;
            }
        };
        let remote_utc_timestamp = remote_item.as_ref().map(|item| item.mod_time.unix_timestamp());
        let db_item = util::await_future(
            SyncItemsEntity::find()
                .filter(SyncItemsColumn::LocalPath.eq(local_path.clone()))
                .filter(SyncItemsColumn::RemotePath.eq(remote_path.clone()))
                .one(db),
        )
        .ok()
        .flatten();

        let push_failed = |model: &mut Model, path: String, err: String| {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(path, err));
        };

        if let Some(db_model) = db_item {
            let update_db_item = |local_timestamp: i32, remote_timestamp: i32| {
                let mut active: SyncItemsActiveModel = db_model.clone().into();
                active.last_local_timestamp = ActiveValue::Set(local_timestamp);
                active.last_remote_timestamp = ActiveValue::Set(remote_timestamp);
                let _ = util::await_future(active.update(db));
            };
            if local_utc_timestamp > db_model.last_local_timestamp as u64
                && remote_utc_timestamp.is_some_and(|remote_timestamp| {
                    remote_timestamp != db_model.last_remote_timestamp as i64
                })
            {
                if let Some(r_item) = remote_item {
                    if !item.path().is_dir() || !r_item.is_dir {
                        record_error(
                            model,
                            shared,
                            &remote.name,
                            sync_dir,
                            SyncError::BothMoreCurrent(local_path.clone(), remote_path.clone()),
                        );
                    }
                }
            } else if local_utc_timestamp > db_model.last_local_timestamp as u64 {
                if let Ok(rclone_item) = push_local(item.file_type().ok(), &item, &local_path, &remote_path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve, remote_item.as_ref()) {
                    let Some(local_ts) = get_local_file_timestamp() else { continue };
                    update_db_item(
                        i32::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                        i32::try_from(rclone_item.mod_time.unix_timestamp()).unwrap_or(db_model.last_remote_timestamp),
                    );
                }
            } else if let Some(remote_timestamp) = remote_utc_timestamp.filter(|remote_timestamp| {
                *remote_timestamp > db_model.last_remote_timestamp as i64
            }) {
                if pull_local(item.file_type().ok(), &item, &local_path, &remote_path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve, remote_item.as_ref()).is_ok() {
                    if let Some(local_ts) = get_local_file_timestamp() {
                        update_db_item(
                            i32::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                            i32::try_from(remote_timestamp).unwrap_or(db_model.last_remote_timestamp),
                        );
                    }
                }
            } else if remote_item.is_none() && local_utc_timestamp == db_model.last_local_timestamp as u64 {
                let removed = if item.path().is_dir() {
                    fs::remove_dir_all(&local_path)
                } else {
                    fs::remove_file(&local_path)
                };
                if let Err(err) = removed {
                    push_failed(model, local_path.clone(), err.to_string());
                    continue;
                }
                delete_db_entry(db, sync_dir.id, &local_path, &remote_path);
            } else if local_utc_timestamp == db_model.last_local_timestamp as u64
                && remote_utc_timestamp.is_some_and(|remote_timestamp| {
                    remote_timestamp == db_model.last_remote_timestamp as i64
                })
            {
            } else if local_utc_timestamp == db_model.last_local_timestamp as u64
                && remote_utc_timestamp.is_some_and(|remote_timestamp| {
                    remote_timestamp < db_model.last_remote_timestamp as i64
                })
            {
                let remote_timestamp = remote_utc_timestamp.unwrap_or(0);
                update_db_item(
                    i32::try_from(local_utc_timestamp).unwrap_or(db_model.last_local_timestamp),
                    i32::try_from(remote_timestamp).unwrap_or(db_model.last_remote_timestamp),
                );
            } else {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path, "Could not decide which copy is newer".into()),
                );
            }
        } else if let Some(remote_timestamp) = remote_utc_timestamp {
            let pushed = if local_utc_timestamp > remote_timestamp as u64 {
                push_local(item.file_type().ok(), &item, &local_path, &remote_path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve, remote_item.as_ref()).is_ok()
            } else {
                pull_local(item.file_type().ok(), &item, &local_path, &remote_path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve, remote_item.as_ref()).is_ok()
            };
            if pushed {
                record_new_item(db, model, shared, remote, sync_dir, &local_path, &remote_path, local_utc_timestamp);
            }
        } else if push_local(item.file_type().ok(), &item, &local_path, &remote_path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve, remote_item.as_ref()).is_ok() {
            record_new_item(db, model, shared, remote, sync_dir, &local_path, &remote_path, local_utc_timestamp);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn push_local(
    file_type: Option<std::fs::FileType>,
    item: &std::fs::DirEntry,
    local_path: &str,
    remote_path: &str,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<Vec<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
    remote_item: Option<&rclone::RcloneRemoteItem>,
) -> Result<rclone::RcloneRemoteItem, ()> {
    let Some(file_type) = file_type else {
        record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path.into(), "Could not read the file type".into()));
        return Err(());
    };
    if let Some(rclone_item) = remote_item {
        let same_type = file_type.is_dir() && rclone_item.is_dir;
        if !same_type {
            if let Err(err) = rclone::sync::purge(&remote.name, remote_path) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path.into(), err.error));
                return Err(());
            }
        }
    }
    if file_type.is_dir() {
        if let Err(err) = rclone::sync::mkdir(&remote.name, remote_path) {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path.into(), err.error));
            return Err(());
        }
        sync_local_directory(item.path().as_path(), remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve);
        note_progress(model, shared, &remote.name, sync_dir, db, tr::tr!("Checking '{}' for changes...", util::fmt_home(local_path)));
    } else if let Err(err) = rclone::sync::copy_to_remote(local_path, &remote.name, remote_path) {
        record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path.into(), err.error));
        return Err(());
    }
    rclone::sync::stat(&remote.name, remote_path).ok().flatten().ok_or(())
}

#[allow(clippy::too_many_arguments)]
fn pull_local(
    file_type: Option<std::fs::FileType>,
    item: &std::fs::DirEntry,
    local_path: &str,
    remote_path: &str,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<Vec<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
    remote_item: Option<&rclone::RcloneRemoteItem>,
) -> Result<(), ()> {
    let Some(file_type) = file_type else {
        return Err(());
    };
    let Some(remote_item) = remote_item else {
        return Err(());
    };
    let same_type = file_type.is_dir() && remote_item.is_dir;
    if !same_type {
        let removed = if file_type.is_dir() {
            fs::remove_dir_all(item.path())
        } else {
            fs::remove_file(item.path())
        };
        if let Err(err) = removed {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path.into(), err.to_string()));
            return Err(());
        }
    }
    if file_type.is_dir() {
        sync_local_directory(item.path().as_path(), remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve);
        note_progress(model, shared, &remote.name, sync_dir, db, tr::tr!("Checking '{}' for changes...", util::fmt_home(local_path)));
    } else if let Err(err) = rclone::sync::copy_to_local(local_path, &remote.name, remote_path) {
        record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path.into(), err.error));
        return Err(());
    }
    Ok(())
}

fn delete_db_entry(db: &DatabaseConnection, sync_dir_id: i32, local_path: &str, remote_path: &str) {
    util::await_future(async {
        if let Some(row) = SyncItemsEntity::find()
            .filter(SyncItemsColumn::SyncDirId.eq(sync_dir_id))
            .filter(SyncItemsColumn::LocalPath.eq(local_path))
            .filter(SyncItemsColumn::RemotePath.eq(remote_path))
            .one(db)
            .await
            .ok()
            .flatten()
        {
            let _ = row.delete(db).await;
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn record_new_item(
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    local_path: &str,
    remote_path: &str,
    local_utc_timestamp: u64,
) {
    let remote_item = match rclone::sync::stat(&remote.name, remote_path) {
        Ok(Some(item)) => item,
        Ok(None) => {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path.into(), "Remote item disappeared after sync".into()));
            return;
        }
        Err(err) => {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path.into(), err.error));
            return;
        }
    };
    let _ = util::await_future(
        SyncItemsActiveModel {
            sync_dir_id: ActiveValue::Set(sync_dir.id),
            local_path: ActiveValue::Set(local_path.to_owned()),
            remote_path: ActiveValue::Set(remote_path.to_owned()),
            last_local_timestamp: ActiveValue::Set(i32::try_from(local_utc_timestamp).unwrap_or(0)),
            last_remote_timestamp: ActiveValue::Set(i32::try_from(remote_item.mod_time.unix_timestamp()).unwrap_or(0)),
            ..Default::default()
        }
        .insert(db),
    );
}

#[allow(clippy::too_many_arguments)]
fn sync_remote_directory(
    remote_dir: &str,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<Vec<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) {
    let mut walk = Walk {
        remote,
        sync_dir,
        db,
        model,
        shared,
        synced,
        rx,
        queue,
        please_resolve,
    };
    checkpoint(&mut walk);
    let Walk {
        remote,
        sync_dir,
        db,
        model,
        shared,
        synced,
        rx,
        queue,
        please_resolve,
    } = walk;

    let ignore_globs = load_ignore_globs(&PathBuf::from(format!(
        "{}/{}",
        sync_dir.local_path, FILE_IGNORE_NAME
    )));
    note_progress(
        model,
        shared,
        &remote.name,
        sync_dir,
        db,
        tr::tr!("Checking '{}' on remote for changes...", remote_dir),
    );
    let items = match rclone::sync::list(&remote.name, remote_dir, false, RcloneListFilter::All) {
        Ok(items) => items,
        Err(err) => {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_dir.to_owned(), err.error));
            return;
        }
    };

    for item in items {
        if shared.quit_requested() {
            break;
        }
        let mut walk = Walk {
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        };
        checkpoint(&mut walk);
        let Walk {
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        } = walk;
        if !sync_dir.exists(db) {
            break;
        }
        if ignore_globs.iter().any(|pattern| pattern.matches(&item.path)) {
            continue;
        }
        let remote_path_string = item.path.clone();
        let Some(local_path_string) = util::local_path_for_remote(&sync_dir.local_path, &sync_dir.remote_path, &item.path) else {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(item.path.clone(), "Remote path is outside the synced folder".into()));
            continue;
        };
        note_progress(model, shared, &remote.name, sync_dir, db, tr::tr!("Checking '{}' on remote for changes...", &remote_path_string));
        if synced.borrow().iter().any(|(local, remote_path)| local == &local_path_string && remote_path == &remote_path_string) {
            continue;
        }
        let local_path = Path::new(&local_path_string);
        let remote_timestamp = item.mod_time.unix_timestamp();
        let local_timestamp = local_path.metadata().ok().and_then(|metadata| {
            metadata.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs())
        });
        let db_item = util::await_future(
            SyncItemsEntity::find()
                .filter(SyncItemsColumn::LocalPath.eq(local_path_string.clone()))
                .filter(SyncItemsColumn::RemotePath.eq(remote_path_string.clone()))
                .one(db),
        )
        .ok()
        .flatten();

        if let Some(db_model) = db_item {
            let update_db_item = |local_ts: i32, remote_ts: i32| {
                let mut active: SyncItemsActiveModel = db_model.clone().into();
                active.last_local_timestamp = ActiveValue::Set(local_ts);
                active.last_remote_timestamp = ActiveValue::Set(remote_ts);
                let _ = util::await_future(active.update(db));
            };
            if local_timestamp.is_some_and(|l_timestamp| {
                l_timestamp > db_model.last_local_timestamp as u64
                    && remote_timestamp != db_model.last_remote_timestamp as i64
            }) {
                if !local_path.is_dir() || !item.is_dir {
                    record_error(model, shared, &remote.name, sync_dir, SyncError::BothMoreCurrent(local_path_string.clone(), remote_path_string.clone()));
                }
                continue;
            } else if local_timestamp
                .is_some_and(|l_timestamp| l_timestamp > db_model.last_local_timestamp as u64)
            {
                if let Some(rclone_item) = push_remote(&item, local_path, &local_path_string, &remote_path_string, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve) {
                    let Some(local_ts) = local_path.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()) else { continue };
                    update_db_item(
                        i32::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                        i32::try_from(rclone_item.mod_time.unix_timestamp()).unwrap_or(db_model.last_remote_timestamp),
                    );
                }
                continue;
            } else if remote_timestamp > db_model.last_remote_timestamp as i64 {
                if pull_remote(&item, local_path, &local_path_string, &remote_path_string, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve) {
                    if let Some(local_ts) = local_path.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()) {
                        update_db_item(
                            i32::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                            i32::try_from(remote_timestamp).unwrap_or(db_model.last_remote_timestamp),
                        );
                    }
                }
            } else if !local_path.exists() && remote_timestamp == db_model.last_remote_timestamp as i64 {
                if let Err(err) = rclone::sync::purge(&remote.name, &remote_path_string) {
                    record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.clone(), err.error));
                    delete_db_entry(db, sync_dir.id, &local_path_string, &remote_path_string);
                }
                continue;
            } else if local_timestamp.is_some_and(|l_timestamp| {
                l_timestamp == db_model.last_local_timestamp as u64
                    && remote_timestamp == db_model.last_remote_timestamp as i64
            }) {
                continue;
            } else if let Some(l_timestamp) = local_timestamp.filter(|l_timestamp| {
                *l_timestamp == db_model.last_local_timestamp as u64
                    && remote_timestamp < db_model.last_remote_timestamp as i64
            }) {
                update_db_item(
                    i32::try_from(l_timestamp).unwrap_or(db_model.last_local_timestamp),
                    i32::try_from(remote_timestamp).unwrap_or(db_model.last_remote_timestamp),
                );
                continue;
            } else {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.clone(), "Could not decide which copy is newer".into()));
                continue;
            }
        } else if let Some(l_timestamp) = local_timestamp {
            let ok = if l_timestamp > remote_timestamp as u64 {
                push_remote(&item, local_path, &local_path_string, &remote_path_string, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve).is_some()
            } else {
                pull_remote(&item, local_path, &local_path_string, &remote_path_string, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve)
            };
            if !ok {
                continue;
            }
        } else if !pull_remote(&item, local_path, &local_path_string, &remote_path_string, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve) {
            continue;
        }

        let Some(l_timestamp) = local_path.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()) else {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path_string.clone(), "Could not read the local modification time".into()));
            continue;
        };
        let r_timestamp = match rclone::sync::stat(&remote.name, &remote_path_string) {
            Ok(Some(found)) => found.mod_time.unix_timestamp(),
            Ok(None) => {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.clone(), "Remote item disappeared after sync".into()));
                continue;
            }
            Err(err) => {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.clone(), err.error));
                continue;
            }
        };
        let _ = util::await_future(
            SyncItemsActiveModel {
                sync_dir_id: ActiveValue::Set(sync_dir.id),
                local_path: ActiveValue::Set(local_path_string.clone()),
                remote_path: ActiveValue::Set(remote_path_string.clone()),
                last_local_timestamp: ActiveValue::Set(i32::try_from(l_timestamp).unwrap_or(0)),
                last_remote_timestamp: ActiveValue::Set(i32::try_from(r_timestamp).unwrap_or(0)),
                ..Default::default()
            }
            .insert(db),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn push_remote(
    item: &rclone::RcloneRemoteItem,
    local_path: &Path,
    local_path_string: &str,
    remote_path_string: &str,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<Vec<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) -> Option<rclone::RcloneRemoteItem> {
    if local_path.is_dir() {
        if !item.is_dir {
            if let Err(err) = rclone::sync::delete(&remote.name, remote_path_string) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.into(), err.error));
                return None;
            }
            if let Err(err) = rclone::sync::mkdir(&remote.name, remote_path_string) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.into(), err.error));
                return None;
            }
        }
        sync_remote_directory(&item.path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve);
    } else {
        if item.is_dir {
            if let Err(err) = rclone::sync::purge(&remote.name, remote_path_string) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.into(), err.error));
                return None;
            }
        }
        if let Err(err) = rclone::sync::copy_to_remote(local_path_string, &remote.name, remote_path_string) {
            record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.into(), err.error));
            return None;
        }
    }
    rclone::sync::stat(&remote.name, remote_path_string).ok().flatten()
}

#[allow(clippy::too_many_arguments)]
fn pull_remote(
    item: &rclone::RcloneRemoteItem,
    local_path: &Path,
    local_path_string: &str,
    remote_path_string: &str,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<Vec<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) -> bool {
    if local_path.exists() {
        if local_path.is_dir() && !item.is_dir {
            if let Err(err) = fs::remove_dir_all(local_path) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path_string.into(), err.to_string()));
                return false;
            }
        } else if !local_path.is_dir() && item.is_dir {
            if let Err(err) = fs::remove_file(local_path) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path_string.into(), err.to_string()));
                return false;
            }
            if let Err(err) = fs::create_dir(local_path) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path_string.into(), err.to_string()));
                return false;
            }
        }
    }
    if item.is_dir {
        if !local_path.exists() {
            if let Err(err) = fs::create_dir(local_path) {
                record_error(model, shared, &remote.name, sync_dir, SyncError::General(local_path_string.into(), err.to_string()));
                return false;
            }
        }
        sync_remote_directory(&item.path, remote, sync_dir, db, model, shared, synced, rx, queue, please_resolve);
    } else if let Err(err) = rclone::sync::copy_to_local(local_path_string, &remote.name, remote_path_string) {
        record_error(model, shared, &remote.name, sync_dir, SyncError::General(remote_path_string.into(), err.error));
        return false;
    }
    true
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
