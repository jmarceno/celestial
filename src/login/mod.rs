//! Create an rclone remote and the matching database row.

mod oauth;
pub mod validate;

use crate::{
    entities::{RemotesActiveModel, RemotesModel},
    login::oauth::AuthType,
    rclone, util,
};
use sea_orm::{ActiveValue, DatabaseConnection, entity::prelude::*};
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;

pub use validate::LoginRequest;

pub fn can_login(config_name: &str) -> Result<(), String> {
    match rclone::sync::stat(config_name, "/") {
        Ok(_) => Ok(()),
        Err(err) => {
            let err_msg = if err.error.contains("Temporary failure in name resolution") {
                tr::tr!(
                    "Unable to connect to the server. Check your internet connection and try again."
                )
            } else if err.error.contains("this account requires a 2FA code") {
                tr::tr!(
                    "A 2FA code is required to log in to this account. Provide one and try again."
                )
            } else {
                tr::tr!(
                    "Unable to authenticate to the server. Check your login credentials and try again."
                )
            };
            Err(err_msg)
        }
    }
}

fn webdav_url(kind: &str, req: &LoginRequest) -> String {
    match kind {
        "nextcloud" | "owncloud" => {
            format!(
                "{}/remote.php/dav/files/{}",
                req.url.trim_end_matches('/'),
                req.username
            )
        }
        _ => req.url.clone(),
    }
}

fn config_body(req: &LoginRequest, token: Option<&str>) -> Result<Value, String> {
    let name = req.name.clone();
    let body = match req.kind.as_str() {
        "dropbox" => json!({
            "name": name,
            "parameters": {
                "client_id": oauth::default_client(AuthType::Dropbox).0,
                "client_secret": oauth::default_client(AuthType::Dropbox).1,
                "token": token.unwrap_or(""),
                "config_refresh_token": false
            },
            "type": "dropbox"
        }),
        "gdrive" => json!({
            "name": name,
            "parameters": {
                "client_id": oauth::default_client(AuthType::GDrive).0,
                "client_secret": oauth::default_client(AuthType::GDrive).1,
                "token": token.unwrap_or(""),
                "config_refresh_token": false
            },
            "type": "drive"
        }),
        "pcloud" => json!({
            "name": name,
            "parameters": {
                "client_id": oauth::default_client(AuthType::PCloud).0,
                "client_secret": oauth::default_client(AuthType::PCloud).1,
                "token": token.unwrap_or(""),
                "config_refresh_token": false
            },
            "type": "pcloud",
            "opt": { "obscure": true }
        }),
        "nextcloud" => json!({
            "name": name,
            "parameters": {
                "url": webdav_url("nextcloud", req),
                "vendor": "nextcloud",
                "user": req.username,
                "pass": req.password
            },
            "type": "webdav",
            "opt": { "obscure": true }
        }),
        "owncloud" => json!({
            "name": name,
            "parameters": {
                "url": webdav_url("owncloud", req),
                "vendor": "owncloud",
                "user": req.username,
                "pass": req.password
            },
            "type": "webdav",
            "opt": { "obscure": true }
        }),
        "webdav" => json!({
            "name": name,
            "parameters": {
                "url": req.url,
                "vendor": "webdav",
                "user": req.username,
                "pass": req.password
            },
            "type": "webdav",
            "opt": { "obscure": true }
        }),
        "protondrive" => json!({
            "name": name,
            "parameters": {
                "username": req.username,
                "password": req.password,
                "2fa": if req.totp_enabled { req.totp.clone() } else { String::new() }
            },
            "type": "protondrive",
            "opt": { "obscure": true }
        }),
        other => return Err(format!("Unknown server type '{other}'.")),
    };
    Ok(body)
}

/// Create the rclone remote, check that it answers, and store it.
///
/// OAuth kinds block this thread until the browser flow finishes or `cancel`
/// is set.
pub fn create_remote(
    db: &DatabaseConnection,
    req: &LoginRequest,
    cancel: &AtomicBool,
    on_auth_wait: impl FnOnce(),
) -> Result<RemotesModel, String> {
    validate::validate(req)?;
    let token = match req.kind.as_str() {
        "dropbox" => Some(oauth::authorize(AuthType::Dropbox, cancel, on_auth_wait)?),
        "gdrive" => Some(oauth::authorize(AuthType::GDrive, cancel, on_auth_wait)?),
        "pcloud" => Some(oauth::authorize(AuthType::PCloud, cancel, on_auth_wait)?),
        _ => {
            drop(on_auth_wait);
            None
        }
    };
    if token.as_deref() == Some("cancelled") || cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return Err("cancelled".into());
    }
    let config_name = req.name.clone();
    let body = config_body(req, token.as_deref())?;
    librclone::rpc("config/create", body.to_string())
        .map_err(|err| format!("Could not save the remote: {err}"))?;

    if let Err(err) = can_login(&config_name) {
        let _ = librclone::rpc("config/delete", json!({ "name": config_name }).to_string());
        return Err(err);
    }

    util::await_future(
        RemotesActiveModel {
            name: ActiveValue::Set(config_name),
            ..Default::default()
        }
        .insert(db),
    )
    .map_err(|err| format!("Could not store the remote: {err}"))
}
