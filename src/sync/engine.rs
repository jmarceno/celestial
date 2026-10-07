//! Two-way synchronization worker. The Qt window communicates through [`Model`].

use super::model::{self, FILE_IGNORE_NAME, Model, SyncError};
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
use sea_orm::TransactionTrait;
use sea_orm::{ActiveValue, Database, DatabaseConnection, entity::prelude::*};
use serde_json::json;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
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
            let failure = match result {
                Ok(Ok(())) => None,
                Ok(Err(message)) => Some(message),
                Err(payload) => Some(panic_message(payload)),
            };
            if let Some(message) = failure {
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
    fs::create_dir_all(&config)
        .map_err(|err| tr::tr!("Unable to create Celeste's config directory [{}].", err))?;
    config.push("rclone.conf");
    librclone::initialize();
    librclone::rpc(
        "config/setpath",
        json!({ "path": config.display().to_string() }).to_string(),
    )
    .map_err(|err| format!("Could not set the rclone config path: {err}"))?;

    let mut db_path = util::get_config_dir();
    db_path.push("celeste.db");
    if !db_path.exists() {
        File::create(&db_path)
            .map_err(|err| tr::tr!("Unable to create Celeste's database file [{}].", err))?;
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
    let please_resolve = format!(
        " {}",
        tr::tr!("Please resolve the reported syncing issues.")
    );

    loop {
        pump(&rx, &mut queue);
        if shared.quit_requested() {
            break;
        }
        apply_deletes(&mut model, &db, &mut queue, shared);
        shared.publish(&mut model);
        apply_between_passes(shared, &db, &mut model, &mut queue, &please_resolve);
        if shared.quit_requested() {
            break;
        }

        let remotes = util::await_future(RemotesEntity::find().all(&db)).unwrap_or_default();
        if remotes.is_empty() {
            model.remotes.clear();
            shared.set_tray(&mut model, tr::tr!("Awaiting sync checks..."), "loading");
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
            apply_deletes(&mut model, &db, &mut queue, shared);
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
                apply_deletes(&mut model, &db, &mut queue, shared);
                if !sync_dir.exists(&db) {
                    continue;
                }
                if model.has_errors(&remote.name, &sync_dir.local_path, &sync_dir.remote_path)
                    || !pair_roots_available(&db, &remote, &sync_dir, &mut model, shared)
                {
                    continue;
                }
                let should_sync = match directory_changed(&db, &remote, &sync_dir) {
                    Ok(changed) => changed,
                    Err(err) => {
                        record_error(
                            &mut model,
                            shared,
                            &remote.name,
                            &sync_dir,
                            SyncError::General(sync_dir.local_path.clone(), err),
                        );
                        continue;
                    }
                };
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
                model.set_phase(
                    &remote.name,
                    &sync_dir.local_path,
                    &sync_dir.remote_path,
                    "syncing",
                );
                model.set_status(
                    &remote.name,
                    &sync_dir.local_path,
                    &sync_dir.remote_path,
                    tr::tr!("Checking for changes..."),
                );
                shared.publish(&mut model);

                let synced: RefCell<HashSet<(String, String)>> = RefCell::new(HashSet::new());
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
                if sync_dir.exists(&db)
                    && !shared.quit_requested()
                    && !model.has_errors(&remote.name, &sync_dir.local_path, &sync_dir.remote_path)
                {
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
                    SyncItemsEntity::delete_many()
                        .filter(SyncItemsColumn::SyncDirId.eq(dir.id))
                        .exec(&tx)
                        .await?;
                    dir.delete(&tx).await?;
                }
            }
            tx.commit().await
        });
        match result {
            Ok(()) => model.remove_dir(&remote_name, &local, &remote_path),
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
                    SyncItemsEntity::delete_many()
                        .filter(SyncItemsColumn::SyncDirId.eq(dir.id))
                        .exec(&tx)
                        .await?;
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
            } => {
                model.dismiss_error(&remote, &local, &remote_path, id, please_resolve);
            }
            Command::ActivateError {
                remote,
                local,
                remote_path,
                id,
            } => activate_error(
                shared,
                db,
                model,
                &remote,
                &local,
                &remote_path,
                id,
                please_resolve,
            ),
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
    // Only act on the versions the person saw when they chose.
    let Some(shown) = model.conflict_versions.remove(&id) else {
        return;
    };
    let current = match observe_conflict(remote_name, local, remote_path, &local_item, &remote_item)
    {
        Ok(current) => current,
        Err(err) => {
            shared.dialog("error", tr::tr!("File Update"), err);
            return;
        }
    };
    if current != shown {
        shared.dialog(
            "info",
            tr::tr!("File Update"),
            tr::tr!(
                "One of the items changed after the question was shown. Open the error again to review the current versions."
            ),
        );
        return;
    }
    let copied = if keep_local {
        copy_local_to_remote(shared, remote_name, &local_item, &remote_item)
    } else {
        copy_remote_to_local(shared, remote_name, &local_item, &remote_item)
    };
    if copied {
        if update_item_timestamps(
            db,
            remote_name,
            local,
            remote_path,
            &local_item,
            &remote_item,
        )
        .is_ok()
        {
            model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
        } else {
            shared.dialog(
                "error",
                "Synchronization history",
                "Could not save the conflict resolution; the error is retained",
            );
        }
    }
}

/// Observe both sides of a conflict, refusing items that are no longer
/// inside the pair or that are reached through a symbolic link.
fn observe_conflict(
    remote_name: &str,
    local_root: &str,
    remote_root: &str,
    local_item: &str,
    remote_item: &str,
) -> Result<(model::ItemVersion, model::ItemVersion), String> {
    if util::local_path_for_remote(local_root, remote_root, remote_item).as_deref()
        != Some(local_item)
    {
        return Err("The conflicting items are no longer inside the synced folder".into());
    }
    validate_local_path(Path::new(local_root), Path::new(local_item))?;
    let local_version = match fs::symlink_metadata(local_item) {
        Ok(metadata) => {
            let timestamp = metadata
                .modified()
                .map_err(|err| err.to_string())?
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_err(|err| err.to_string())?
                .as_secs();
            Some((
                metadata.is_dir(),
                i64::try_from(timestamp).map_err(|err| err.to_string())?,
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.to_string()),
    };
    let remote_version = rclone::sync::stat(remote_name, remote_item)
        .map_err(|err| {
            tr::tr!(
                "Unable to fetch data for '{}' from the remote.",
                remote_item
            ) + "\n\n"
                + &err.error
        })?
        .map(|item| (item.is_dir, item.mod_time.unix_timestamp()));
    Ok((local_version, remote_version))
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
    let versions = match observe_conflict(remote_name, local, remote_path, local_item, remote_item)
    {
        Ok(versions) => versions,
        Err(err) => {
            shared.dialog("error", tr::tr!("File Update"), err);
            return;
        }
    };
    if matches!(versions, (Some((true, _)), _) | (_, Some((true, _)))) {
        shared.dialog(
            "error",
            tr::tr!("File Update"),
            tr::tr!(
                "A file and a folder share this name. Rename or remove one of them, then dismiss this error."
            ),
        );
        model.conflict_versions.remove(&id);
        return;
    }
    let copied = match versions {
        (None, None) => {
            shared.dialog(
                "info",
                tr::tr!("File Update"),
                tr::tr!("Neither the local item or remote item exists anymore. This error will now be removed."),
            );
            model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
            return;
        }
        (Some(_), None) => {
            shared.dialog(
                "info",
                tr::tr!("File Update"),
                tr::tr!("Only the local item exists now, so it will be synced to the remote."),
            );
            copy_local_to_remote(shared, remote_name, local_item, remote_item)
        }
        (None, Some(_)) => {
            shared.dialog(
                "info",
                tr::tr!("File Update"),
                tr::tr!(
                    "Only the remote item exists now, so it will be synced to the local machine."
                ),
            );
            copy_remote_to_local(shared, remote_name, local_item, remote_item)
        }
        (Some(_), Some(_)) => {
            model.conflict_versions.insert(id, versions);
            shared.dialog(
                "conflict",
                tr::tr!(
                    "Both the local item '{}' and remote item '{}' have been updated since the last sync.",
                    util::fmt_home(local_item),
                    remote_item
                ),
                error_ref(remote_name, local, remote_path, id),
            );
            return;
        }
    };
    if copied {
        if update_item_timestamps(db, remote_name, local, remote_path, local_item, remote_item)
            .is_ok()
        {
            model.dismiss_error(remote_name, local, remote_path, id, please_resolve);
        } else {
            shared.dialog(
                "error",
                "Synchronization history",
                "Could not save the conflict resolution; the error is retained",
            );
        }
    }
}

fn copy_local_to_remote(
    shared: &Shared,
    remote_name: &str,
    local_item: &str,
    remote_item: &str,
) -> bool {
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

fn copy_remote_to_local(
    shared: &Shared,
    remote_name: &str,
    local_item: &str,
    remote_item: &str,
) -> bool {
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
    local_root: &str,
    remote_root: &str,
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
    let remote = util::await_future(
        RemotesEntity::find()
            .filter(RemotesColumn::Name.eq(remote_name))
            .one(db),
    )
    .map_err(|_| ())?
    .ok_or(())?;
    let sync_dir = util::await_future(
        SyncDirsEntity::find()
            .filter(SyncDirsColumn::RemoteId.eq(remote.id))
            .filter(SyncDirsColumn::LocalPath.eq(local_root))
            .filter(SyncDirsColumn::RemotePath.eq(remote_root))
            .one(db),
    )
    .map_err(|_| ())?
    .ok_or(())?;
    let model = util::await_future(
        SyncItemsEntity::find()
            .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
            .filter(SyncItemsColumn::LocalPath.eq(local_item))
            .filter(SyncItemsColumn::RemotePath.eq(remote_item))
            .one(db),
    )
    .map_err(|_| ())?
    .ok_or(())?;
    let mut active: SyncItemsActiveModel = model.into();
    active.last_local_timestamp = ActiveValue::Set(local_timestamp.try_into().map_err(|_| ())?);
    active.last_remote_timestamp = ActiveValue::Set(remote_timestamp);
    util::await_future(active.update(db)).map_err(|_| ())?;
    Ok(())
}

fn pair_roots_available(
    db: &DatabaseConnection,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    model: &mut Model,
    shared: &Shared,
) -> bool {
    if !sync_dir.exists(db) {
        return false;
    }
    if let Err(err) = fs::read_dir(&sync_dir.local_path) {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(
                sync_dir.local_path.clone(),
                format!("Local sync folder is unavailable: {err}"),
            ),
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
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(sync_dir.remote_path.clone(), message),
            );
            false
        }
    }
}

fn directory_changed(
    db: &DatabaseConnection,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
) -> Result<bool, String> {
    // A complete observation uses one DB read and one recursive remote listing.
    let rows = util::await_future(
        SyncItemsEntity::find()
            .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
            .all(db),
    )
    .map_err(|err| err.to_string())?;
    let local_items = local_tree(Path::new(&sync_dir.local_path)).map_err(|err| err.to_string())?;
    let remote_items = rclone::sync::list(
        &remote.name,
        &sync_dir.remote_path,
        true,
        RcloneListFilter::All,
    )
    .map_err(|err| err.error)?;
    let ignore_globs = load_ignore_globs(&Path::new(&sync_dir.local_path).join(FILE_IGNORE_NAME))?;
    let local_rows: HashMap<_, _> = rows
        .iter()
        .map(|row| (row.local_path.as_str(), row))
        .collect();
    let remote_rows: HashMap<_, _> = rows
        .iter()
        .map(|row| (row.remote_path.as_str(), row))
        .collect();
    let mut local_items_kept = Vec::new();
    for (path, metadata) in local_items {
        let path = path
            .into_os_string()
            .into_string()
            .map_err(|_| "Local path is not valid UTF-8")?;
        let relative = Path::new(&path)
            .strip_prefix(&sync_dir.local_path)
            .map_err(|err| err.to_string())?;
        if !is_ignored(&relative.to_string_lossy(), &ignore_globs) {
            local_items_kept.push((path, metadata));
        }
    }
    let mut remote_items_kept = Vec::new();
    for item in remote_items {
        let local_path =
            util::local_path_for_remote(&sync_dir.local_path, &sync_dir.remote_path, &item.path)
                .ok_or("Remote path is outside the synced folder")?;
        let relative = Path::new(&local_path)
            .strip_prefix(&sync_dir.local_path)
            .map_err(|err| err.to_string())?;
        if !is_ignored(&relative.to_string_lossy(), &ignore_globs) {
            remote_items_kept.push(item);
        }
    }
    // An unmounted drive or a provider returning an empty listing looks like a
    // folder whose every item was deleted. Never propagate that wholesale.
    let local_tracked = local_items_kept
        .iter()
        .any(|(path, _)| local_rows.contains_key(path.as_str()));
    let remote_tracked = remote_items_kept
        .iter()
        .any(|item| remote_rows.contains_key(item.path.as_str()));
    if local_items_kept.is_empty() && remote_tracked {
        return Err("Every synced item is missing from the local folder while the remote still has them. Automatic deletion is paused; check that the local folder or its drive is mounted. To delete everything intentionally, remove the items on both sides.".into());
    }
    if remote_items_kept.is_empty() && local_tracked {
        return Err("Every synced item is missing from the remote folder while the local folder still has them. Automatic deletion is paused; check the remote. To delete everything intentionally, remove the items on both sides.".into());
    }
    let mut local_seen = HashSet::new();
    let mut remote_seen = HashSet::new();
    for (path, metadata) in local_items_kept {
        let Some(row) = local_rows.get(path.as_str()) else {
            return Ok(true);
        };
        let timestamp = metadata
            .modified()
            .map_err(|err| err.to_string())?
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(|err| err.to_string())?
            .as_secs();
        if i64::try_from(timestamp).ok() != Some(row.last_local_timestamp) {
            return Ok(true);
        }
        local_seen.insert(path);
    }
    for item in remote_items_kept {
        let Some(row) = remote_rows.get(item.path.as_str()) else {
            return Ok(true);
        };
        if item.mod_time.unix_timestamp() != row.last_remote_timestamp {
            return Ok(true);
        }
        remote_seen.insert(item.path);
    }
    let mut absent = Vec::new();
    for row in &rows {
        let relative = Path::new(&row.local_path)
            .strip_prefix(&sync_dir.local_path)
            .map_err(|err| err.to_string())?;
        if is_ignored(&relative.to_string_lossy(), &ignore_globs) {
            continue;
        }
        let local_exists = local_seen.contains(&row.local_path);
        let remote_exists = remote_seen.contains(&row.remote_path);
        if local_exists != remote_exists {
            return Ok(true);
        }
        if !local_exists {
            absent.push(row.id);
        }
    }
    // Retire history only after BOTH complete observations succeeded. This
    // prevents a later recreated file from being mistaken for an old deletion.
    if !absent.is_empty() {
        util::await_future(
            SyncItemsEntity::delete_many()
                .filter(SyncItemsColumn::Id.is_in(absent))
                .exec(db),
        )
        .map_err(|err| err.to_string())?;
    }
    Ok(false)
}

// Read metadata without opening file contents or interpreting folder names as globs.
// Symlinks are not followed: paths inside a pair must stay inside that pair.
fn local_tree(root: &Path) -> Result<Vec<(PathBuf, fs::Metadata)>, std::io::Error> {
    let mut pending = vec![root.to_path_buf()];
    let mut items = Vec::new();
    while let Some(dir) = pending.pop() {
        for item in fs::read_dir(dir)? {
            let item = item?;
            let metadata = fs::symlink_metadata(item.path())?;
            if metadata.is_dir() {
                pending.push(item.path());
            }
            items.push((item.path(), metadata));
        }
    }
    Ok(items)
}

fn load_ignore_globs(ignore_file: &Path) -> Result<Vec<glob::Pattern>, String> {
    match fs::metadata(ignore_file) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.to_string()),
        Ok(metadata) if !metadata.is_file() => {
            return Err("Exclusion rules are not a regular file".into());
        }
        Ok(_) => {}
    }
    let _lock =
        file_lock::FileLock::lock(ignore_file, true, file_lock::FileOptions::new().read(true))
            .map_err(|err| err.to_string())?;
    fs::read_to_string(ignore_file)
        .map_err(|err| err.to_string())?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| glob::Pattern::new(line).map_err(|err| format!("{line}: {err}")))
        .collect()
}

fn is_ignored(relative: &str, patterns: &[glob::Pattern]) -> bool {
    if relative == FILE_IGNORE_NAME {
        return true;
    }
    let mut path = relative;
    loop {
        if patterns.iter().any(|pattern| pattern.matches(path)) {
            return true;
        }
        let Some((parent, _)) = path.rsplit_once('/') else {
            return false;
        };
        path = parent;
    }
}

fn validate_local_path(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path.strip_prefix(root).map_err(|err| err.to_string())?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Symbolic links inside a synced folder are not followed".into());
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(())
}

// Directory mtimes do not prove that their contents are unchanged. Before a
// recursive deletion, require every descendant to have an unchanged baseline.
fn remove_unchanged_local(
    db: &DatabaseConnection,
    sync_dir: &SyncDirsModel,
    root: &Path,
) -> std::io::Result<()> {
    let rows = util::await_future(
        SyncItemsEntity::find()
            .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
            .all(db),
    )
    .map_err(std::io::Error::other)?;
    let rows: HashMap<_, _> = rows
        .iter()
        .map(|row| (row.local_path.as_str(), row))
        .collect();
    let patterns = load_ignore_globs(&Path::new(&sync_dir.local_path).join(FILE_IGNORE_NAME))
        .map_err(std::io::Error::other)?;
    for (path, metadata) in local_tree(root)? {
        let path_text = path
            .to_str()
            .ok_or_else(|| std::io::Error::other("Non-UTF-8 path"))?;
        let relative = path
            .strip_prefix(&sync_dir.local_path)
            .map_err(std::io::Error::other)?;
        let unchanged = rows.get(path_text).is_some_and(|row| {
            metadata.is_dir()
                || metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .is_some_and(|time| {
                        i64::try_from(time.as_secs()).ok() == Some(row.last_local_timestamp)
                    })
        });
        if !unchanged
            || metadata.file_type().is_symlink()
            || is_ignored(&relative.to_string_lossy(), &patterns)
        {
            return Err(std::io::Error::other(
                "Folder contains changed, untracked or excluded items; automatic deletion is paused",
            ));
        }
    }
    fs::remove_dir_all(root)
}

fn purge_unchanged_remote(
    db: &DatabaseConnection,
    sync_dir: &SyncDirsModel,
    remote: &RemotesModel,
    root: &str,
) -> Result<(), rclone::RcloneError> {
    let rows = util::await_future(
        SyncItemsEntity::find()
            .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
            .all(db),
    )
    .map_err(|err| rclone::RcloneError {
        error: err.to_string(),
    })?;
    let rows: HashMap<_, _> = rows
        .iter()
        .map(|row| (row.remote_path.as_str(), row))
        .collect();
    let patterns = load_ignore_globs(&Path::new(&sync_dir.local_path).join(FILE_IGNORE_NAME))
        .map_err(|error| rclone::RcloneError { error })?;
    for item in rclone::sync::list(&remote.name, root, true, RcloneListFilter::All)? {
        let local_path =
            util::local_path_for_remote(&sync_dir.local_path, &sync_dir.remote_path, &item.path)
                .ok_or_else(|| rclone::RcloneError {
                    error: "Remote path is outside the synced folder".into(),
                })?;
        let relative = Path::new(&local_path)
            .strip_prefix(&sync_dir.local_path)
            .map_err(|err| rclone::RcloneError {
                error: err.to_string(),
            })?;
        let unchanged = rows.get(item.path.as_str()).is_some_and(|row| {
            item.is_dir || item.mod_time.unix_timestamp() == row.last_remote_timestamp
        });
        if !unchanged || is_ignored(&relative.to_string_lossy(), &patterns) {
            return Err(rclone::RcloneError { error: "Folder contains changed, untracked or excluded items; automatic deletion is paused".into() });
        }
    }
    rclone::sync::purge(&remote.name, root)
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
    synced: &'a RefCell<HashSet<(String, String)>>,
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
    process_deletions(walk.model, walk.db, walk.shared);
}

#[allow(clippy::too_many_arguments)]
fn sync_local_directory(
    local_dir: &Path,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<HashSet<(String, String)>>,
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
    let ignore_globs =
        match load_ignore_globs(&Path::new(&sync_dir.local_path).join(FILE_IGNORE_NAME)) {
            Ok(patterns) => patterns,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(FILE_IGNORE_NAME.into(), err),
                );
                return;
            }
        };

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
        if !sync_dir.exists(db) || shared.quit_requested() {
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
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        if !file_type.is_file() && !file_type.is_dir() {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(
                    item.path().display().to_string(),
                    "Only regular files and directories can be synchronized".into(),
                ),
            );
            continue;
        }
        let local_path = item.path().to_str().unwrap_or("").to_owned();
        let stripped_remote_path = item
            .path()
            .strip_prefix(&sync_dir.local_path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let remote_path = if sync_dir.remote_path.is_empty() {
            stripped_remote_path.clone()
        } else {
            format!("{}/{}", sync_dir.remote_path, stripped_remote_path)
        };
        note_progress(
            model,
            shared,
            &remote.name,
            sync_dir,
            db,
            tr::tr!("Checking '{}' for changes...", util::fmt_home(&local_path)),
        );
        if is_ignored(&stripped_remote_path, &ignore_globs) {
            continue;
        }
        if !synced
            .borrow_mut()
            .insert((local_path.clone(), remote_path.clone()))
        {
            continue;
        }

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
                SyncError::General(
                    local_path,
                    "Could not read the local modification time".into(),
                ),
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
        if item.file_type().is_ok_and(|kind| kind.is_dir())
            && remote_item.as_ref().is_some_and(|remote| remote.is_dir)
        {
            sync_local_directory(
                &item.path(),
                remote,
                sync_dir,
                db,
                model,
                shared,
                synced,
                rx,
                queue,
                please_resolve,
            );
        }
        let remote_utc_timestamp = remote_item
            .as_ref()
            .map(|item| item.mod_time.unix_timestamp());
        let db_item = match util::await_future(
            SyncItemsEntity::find()
                .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                .filter(SyncItemsColumn::LocalPath.eq(local_path.clone()))
                .filter(SyncItemsColumn::RemotePath.eq(remote_path.clone()))
                .one(db),
        ) {
            Ok(item) => item,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path.clone(), err.to_string()),
                );
                continue;
            }
        };

        let push_failed = |model: &mut Model, path: String, err: String| {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(path, err),
            );
        };

        if let Some(db_model) = db_item {
            // Any difference from the baseline is a change, including a
            // timestamp that moved backwards because an older version was restored.
            let local_changed = local_utc_timestamp != db_model.last_local_timestamp as u64;
            if local_changed
                && remote_utc_timestamp.is_some_and(|remote_timestamp| {
                    remote_timestamp != db_model.last_remote_timestamp
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
            } else if local_changed {
                if let Ok(rclone_item) = push_local(
                    item.file_type().ok(),
                    &item,
                    &local_path,
                    &remote_path,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                    remote_item.as_ref(),
                ) {
                    let Some(local_ts) = get_local_file_timestamp() else {
                        continue;
                    };
                    save_baseline(
                        db,
                        model,
                        shared,
                        remote,
                        sync_dir,
                        &db_model,
                        i64::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                        rclone_item.mod_time.unix_timestamp(),
                    );
                }
            } else if let Some(remote_timestamp) = remote_utc_timestamp
                .filter(|remote_timestamp| *remote_timestamp != db_model.last_remote_timestamp)
            {
                if pull_local(
                    item.file_type().ok(),
                    &item,
                    &local_path,
                    &remote_path,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                    remote_item.as_ref(),
                )
                .is_ok()
                {
                    if let Some(local_ts) = get_local_file_timestamp() {
                        save_baseline(
                            db,
                            model,
                            shared,
                            remote,
                            sync_dir,
                            &db_model,
                            i64::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                            remote_timestamp,
                        );
                    }
                }
            } else if remote_item.is_none() {
                let removed = if item.path().is_dir() {
                    remove_unchanged_local(db, sync_dir, Path::new(&local_path))
                } else {
                    fs::remove_file(&local_path)
                };
                if let Err(err) = removed {
                    push_failed(model, local_path.clone(), err.to_string());
                    continue;
                }
                delete_db_entry(db, sync_dir.id, &local_path, &remote_path);
            }
        } else if let Some(remote_timestamp) = remote_utc_timestamp {
            let pushed = if local_utc_timestamp > remote_timestamp as u64 {
                push_local(
                    item.file_type().ok(),
                    &item,
                    &local_path,
                    &remote_path,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                    remote_item.as_ref(),
                )
                .is_ok()
            } else {
                pull_local(
                    item.file_type().ok(),
                    &item,
                    &local_path,
                    &remote_path,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                    remote_item.as_ref(),
                )
                .is_ok()
            };
            if pushed {
                record_new_item(
                    db,
                    model,
                    shared,
                    remote,
                    sync_dir,
                    &local_path,
                    &remote_path,
                    local_utc_timestamp,
                );
            }
        } else if push_local(
            item.file_type().ok(),
            &item,
            &local_path,
            &remote_path,
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
            remote_item.as_ref(),
        )
        .is_ok()
        {
            record_new_item(
                db,
                model,
                shared,
                remote,
                sync_dir,
                &local_path,
                &remote_path,
                local_utc_timestamp,
            );
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
    synced: &RefCell<HashSet<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
    remote_item: Option<&rclone::RcloneRemoteItem>,
) -> Result<rclone::RcloneRemoteItem, ()> {
    let Some(file_type) = file_type else {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(local_path.into(), "Could not read the file type".into()),
        );
        return Err(());
    };
    if let Some(rclone_item) = remote_item {
        let same_type = file_type.is_dir() == rclone_item.is_dir;
        if !same_type {
            let removed = if rclone_item.is_dir {
                purge_unchanged_remote(db, sync_dir, remote, remote_path)
            } else {
                rclone::sync::delete(&remote.name, remote_path)
            };
            if let Err(err) = removed {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(remote_path.into(), err.error),
                );
                return Err(());
            }
        }
    }
    if file_type.is_dir() {
        if let Err(err) = rclone::sync::mkdir(&remote.name, remote_path) {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(remote_path.into(), err.error),
            );
            return Err(());
        }
        sync_local_directory(
            item.path().as_path(),
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        );
        note_progress(
            model,
            shared,
            &remote.name,
            sync_dir,
            db,
            tr::tr!("Checking '{}' for changes...", util::fmt_home(local_path)),
        );
    } else if let Err(err) = rclone::sync::copy_to_remote(local_path, &remote.name, remote_path) {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(local_path.into(), err.error),
        );
        return Err(());
    }
    rclone::sync::stat(&remote.name, remote_path)
        .ok()
        .flatten()
        .ok_or(())
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
    synced: &RefCell<HashSet<(String, String)>>,
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
    let same_type = file_type.is_dir() == remote_item.is_dir;
    if !same_type {
        let removed = if file_type.is_dir() {
            remove_unchanged_local(db, sync_dir, &item.path())
        } else {
            fs::remove_file(item.path())
        };
        if let Err(err) = removed {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(local_path.into(), err.to_string()),
            );
            return Err(());
        }
    }
    if remote_item.is_dir {
        fs::create_dir_all(item.path()).map_err(|err| {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(local_path.into(), err.to_string()),
            );
        })?;
        sync_local_directory(
            item.path().as_path(),
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        );
        note_progress(
            model,
            shared,
            &remote.name,
            sync_dir,
            db,
            tr::tr!("Checking '{}' for changes...", util::fmt_home(local_path)),
        );
    } else if let Err(err) = rclone::sync::copy_to_local(local_path, &remote.name, remote_path) {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(remote_path.into(), err.error),
        );
        return Err(());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn save_baseline(
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    row: &crate::entities::SyncItemsModel,
    local_timestamp: i64,
    remote_timestamp: i64,
) {
    let mut active: SyncItemsActiveModel = row.clone().into();
    active.last_local_timestamp = ActiveValue::Set(local_timestamp);
    active.last_remote_timestamp = ActiveValue::Set(remote_timestamp);
    if let Err(err) = util::await_future(active.update(db)) {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(
                row.local_path.clone(),
                format!("Could not save synchronization history: {err}"),
            ),
        );
    }
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
    _local_utc_timestamp: u64,
) {
    let Some(local_utc_timestamp) = fs::metadata(local_path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
    else {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(
                local_path.into(),
                "Could not read the local modification time after sync".into(),
            ),
        );
        return;
    };
    let remote_item = match rclone::sync::stat(&remote.name, remote_path) {
        Ok(Some(item)) => item,
        Ok(None) => {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(
                    remote_path.into(),
                    "Remote item disappeared after sync".into(),
                ),
            );
            return;
        }
        Err(err) => {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(remote_path.into(), err.error),
            );
            return;
        }
    };
    if let Err(err) = util::await_future(
        SyncItemsActiveModel {
            sync_dir_id: ActiveValue::Set(sync_dir.id),
            local_path: ActiveValue::Set(local_path.to_owned()),
            remote_path: ActiveValue::Set(remote_path.to_owned()),
            last_local_timestamp: ActiveValue::Set(i64::try_from(local_utc_timestamp).unwrap_or(0)),
            last_remote_timestamp: ActiveValue::Set(remote_item.mod_time.unix_timestamp()),
            ..Default::default()
        }
        .insert(db),
    ) {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(
                local_path.to_owned(),
                format!("Could not save synchronization history: {err}"),
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_remote_directory(
    remote_dir: &str,
    remote: &RemotesModel,
    sync_dir: &SyncDirsModel,
    db: &DatabaseConnection,
    model: &mut Model,
    shared: &Shared,
    synced: &RefCell<HashSet<(String, String)>>,
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

    let ignore_globs =
        match load_ignore_globs(&Path::new(&sync_dir.local_path).join(FILE_IGNORE_NAME)) {
            Ok(patterns) => patterns,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(FILE_IGNORE_NAME.into(), err),
                );
                return;
            }
        };
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
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(remote_dir.to_owned(), err.error),
            );
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
        if !sync_dir.exists(db) || shared.quit_requested() {
            break;
        }
        let remote_path_string = item.path.clone();
        let Some(local_path_string) =
            util::local_path_for_remote(&sync_dir.local_path, &sync_dir.remote_path, &item.path)
        else {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(
                    item.path.clone(),
                    "Remote path is outside the synced folder".into(),
                ),
            );
            continue;
        };
        let relative = Path::new(&local_path_string)
            .strip_prefix(&sync_dir.local_path)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        if is_ignored(&relative, &ignore_globs) {
            continue;
        }
        note_progress(
            model,
            shared,
            &remote.name,
            sync_dir,
            db,
            tr::tr!(
                "Checking '{}' on remote for changes...",
                &remote_path_string
            ),
        );
        if let Err(err) = validate_local_path(
            Path::new(&sync_dir.local_path),
            Path::new(&local_path_string),
        ) {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(local_path_string.clone(), err),
            );
            continue;
        }
        if item.is_dir && Path::new(&local_path_string).is_dir() {
            sync_remote_directory(
                &item.path,
                remote,
                sync_dir,
                db,
                model,
                shared,
                synced,
                rx,
                queue,
                please_resolve,
            );
        }
        if synced
            .borrow()
            .contains(&(local_path_string.clone(), remote_path_string.clone()))
        {
            continue;
        }
        let local_path = Path::new(&local_path_string);
        let remote_timestamp = item.mod_time.unix_timestamp();
        if let Err(err) = validate_local_path(Path::new(&sync_dir.local_path), local_path) {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(local_path_string.clone(), err),
            );
            continue;
        }
        let local_timestamp = match fs::metadata(local_path) {
            Ok(metadata) => match metadata.modified().and_then(|time| {
                time.duration_since(SystemTime::UNIX_EPOCH)
                    .map_err(std::io::Error::other)
            }) {
                Ok(time) => Some(time.as_secs()),
                Err(err) => {
                    record_error(
                        model,
                        shared,
                        &remote.name,
                        sync_dir,
                        SyncError::General(local_path_string.clone(), err.to_string()),
                    );
                    continue;
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path_string.clone(), err.to_string()),
                );
                continue;
            }
        };
        let db_item = match util::await_future(
            SyncItemsEntity::find()
                .filter(SyncItemsColumn::SyncDirId.eq(sync_dir.id))
                .filter(SyncItemsColumn::LocalPath.eq(local_path_string.clone()))
                .filter(SyncItemsColumn::RemotePath.eq(remote_path_string.clone()))
                .one(db),
        ) {
            Ok(item) => item,
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path_string.clone(), err.to_string()),
                );
                continue;
            }
        };

        if let Some(db_model) = db_item {
            // Any difference from the baseline is a change, including a
            // timestamp that moved backwards because an older version was restored.
            let local_changed = local_timestamp
                .is_some_and(|l_timestamp| l_timestamp != db_model.last_local_timestamp as u64);
            let remote_changed = remote_timestamp != db_model.last_remote_timestamp;
            if local_changed && remote_changed {
                if !local_path.is_dir() || !item.is_dir {
                    record_error(
                        model,
                        shared,
                        &remote.name,
                        sync_dir,
                        SyncError::BothMoreCurrent(
                            local_path_string.clone(),
                            remote_path_string.clone(),
                        ),
                    );
                }
                continue;
            } else if local_changed {
                if let Some(rclone_item) = push_remote(
                    &item,
                    local_path,
                    &local_path_string,
                    &remote_path_string,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                ) {
                    let Some(local_ts) = local_path
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                    else {
                        continue;
                    };
                    save_baseline(
                        db,
                        model,
                        shared,
                        remote,
                        sync_dir,
                        &db_model,
                        i64::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                        rclone_item.mod_time.unix_timestamp(),
                    );
                }
                continue;
            } else if remote_changed {
                if pull_remote(
                    &item,
                    local_path,
                    &local_path_string,
                    &remote_path_string,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                ) {
                    if let Some(local_ts) = local_path
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                    {
                        save_baseline(
                            db,
                            model,
                            shared,
                            remote,
                            sync_dir,
                            &db_model,
                            i64::try_from(local_ts).unwrap_or(db_model.last_local_timestamp),
                            remote_timestamp,
                        );
                    }
                }
                continue;
            } else if !local_path.exists() {
                let result = if item.is_dir {
                    purge_unchanged_remote(db, sync_dir, remote, &remote_path_string)
                } else {
                    rclone::sync::delete(&remote.name, &remote_path_string)
                };
                match result {
                    Ok(()) => {
                        delete_db_entry(db, sync_dir.id, &local_path_string, &remote_path_string)
                    }
                    Err(err) => record_error(
                        model,
                        shared,
                        &remote.name,
                        sync_dir,
                        SyncError::General(remote_path_string.clone(), err.error),
                    ),
                }
                continue;
            } else {
                continue;
            }
        } else if let Some(l_timestamp) = local_timestamp {
            let ok = if l_timestamp > remote_timestamp as u64 {
                push_remote(
                    &item,
                    local_path,
                    &local_path_string,
                    &remote_path_string,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                )
                .is_some()
            } else {
                pull_remote(
                    &item,
                    local_path,
                    &local_path_string,
                    &remote_path_string,
                    remote,
                    sync_dir,
                    db,
                    model,
                    shared,
                    synced,
                    rx,
                    queue,
                    please_resolve,
                )
            };
            if !ok {
                continue;
            }
        } else if !pull_remote(
            &item,
            local_path,
            &local_path_string,
            &remote_path_string,
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        ) {
            continue;
        }

        let Some(l_timestamp) = local_path
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
        else {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(
                    local_path_string.clone(),
                    "Could not read the local modification time".into(),
                ),
            );
            continue;
        };
        let r_timestamp = match rclone::sync::stat(&remote.name, &remote_path_string) {
            Ok(Some(found)) => found.mod_time.unix_timestamp(),
            Ok(None) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(
                        remote_path_string.clone(),
                        "Remote item disappeared after sync".into(),
                    ),
                );
                continue;
            }
            Err(err) => {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(remote_path_string.clone(), err.error),
                );
                continue;
            }
        };
        if let Err(err) = util::await_future(
            SyncItemsActiveModel {
                sync_dir_id: ActiveValue::Set(sync_dir.id),
                local_path: ActiveValue::Set(local_path_string.clone()),
                remote_path: ActiveValue::Set(remote_path_string.clone()),
                last_local_timestamp: ActiveValue::Set(i64::try_from(l_timestamp).unwrap_or(0)),
                last_remote_timestamp: ActiveValue::Set(r_timestamp),
                ..Default::default()
            }
            .insert(db),
        ) {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(
                    local_path_string.clone(),
                    format!("Could not save synchronization history: {err}"),
                ),
            );
        }
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
    synced: &RefCell<HashSet<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) -> Option<rclone::RcloneRemoteItem> {
    if local_path.is_dir() {
        if !item.is_dir {
            if let Err(err) = rclone::sync::delete(&remote.name, remote_path_string) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(remote_path_string.into(), err.error),
                );
                return None;
            }
            if let Err(err) = rclone::sync::mkdir(&remote.name, remote_path_string) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(remote_path_string.into(), err.error),
                );
                return None;
            }
        }
        sync_remote_directory(
            &item.path,
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        );
    } else {
        if item.is_dir {
            if let Err(err) = purge_unchanged_remote(db, sync_dir, remote, remote_path_string) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(remote_path_string.into(), err.error),
                );
                return None;
            }
        }
        if let Err(err) =
            rclone::sync::copy_to_remote(local_path_string, &remote.name, remote_path_string)
        {
            record_error(
                model,
                shared,
                &remote.name,
                sync_dir,
                SyncError::General(remote_path_string.into(), err.error),
            );
            return None;
        }
    }
    rclone::sync::stat(&remote.name, remote_path_string)
        .ok()
        .flatten()
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
    synced: &RefCell<HashSet<(String, String)>>,
    rx: &crossbeam_channel::Receiver<Command>,
    queue: &mut VecDeque<Command>,
    please_resolve: &str,
) -> bool {
    if local_path.exists() {
        if local_path.is_dir() && !item.is_dir {
            if let Err(err) = remove_unchanged_local(db, sync_dir, local_path) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path_string.into(), err.to_string()),
                );
                return false;
            }
        } else if !local_path.is_dir() && item.is_dir {
            if let Err(err) = fs::remove_file(local_path) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path_string.into(), err.to_string()),
                );
                return false;
            }
            if let Err(err) = fs::create_dir(local_path) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path_string.into(), err.to_string()),
                );
                return false;
            }
        }
    }
    if item.is_dir {
        if !local_path.exists() {
            if let Err(err) = fs::create_dir(local_path) {
                record_error(
                    model,
                    shared,
                    &remote.name,
                    sync_dir,
                    SyncError::General(local_path_string.into(), err.to_string()),
                );
                return false;
            }
        }
        sync_remote_directory(
            &item.path,
            remote,
            sync_dir,
            db,
            model,
            shared,
            synced,
            rx,
            queue,
            please_resolve,
        );
    } else if let Err(err) =
        rclone::sync::copy_to_local(local_path_string, &remote.name, remote_path_string)
    {
        record_error(
            model,
            shared,
            &remote.name,
            sync_dir,
            SyncError::General(remote_path_string.into(), err.error),
        );
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

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
