//! Field checks that used to live on the GTK login rows.

use crate::rclone;
use regex::Regex;
use url::Url;

#[derive(Clone, Debug)]
pub struct LoginRequest {
    pub kind: String,
    pub name: String,
    pub url: String,
    pub username: String,
    pub password: String,
    pub totp: String,
    pub totp_enabled: bool,
}

pub fn validate_name(name: &str) -> Result<(), String> {
    let existing: Vec<String> = rclone::get_remotes()
        .iter()
        .map(|config| config.remote_name())
        .collect();
    if existing.iter().any(|n| n == name) {
        return Err(tr::tr!("Name already exists."));
    }
    let ok = Regex::new(r"^[0-9a-zA-Z_.][0-9a-zA-Z_. -]*[0-9a-zA-Z_.-]$")
        .unwrap()
        .is_match(name);
    if !ok {
        return Err(tr::tr!(
            "Invalid name. Names must:\n- Only contain numbers, letters, '_', '-', '.', and spaces\n- Not start with '-' or a space\n- Not end with a space"
        ));
    }
    Ok(())
}

pub fn validate_url(text: &str, disallow_nextcloud_suffix: bool) -> Result<(), String> {
    let url = Url::parse(text).map_err(|err| tr::tr!("Invalid server URL ({}).", err))?;
    if !url.has_host() {
        return Err(tr::tr!("Invalid server URL (no domain specified)."));
    }
    if url.password().is_some() {
        return Err(tr::tr!("Invalid server URL (password was specified)."));
    }
    if !["http", "https"].contains(&url.scheme()) {
        return Err(tr::tr!(
            "Invalid server URL(unknown server scheme {}).",
            url.scheme()
        ));
    }
    if disallow_nextcloud_suffix && url.path().contains("/remote.php/") {
        let text_to_remove = Regex::new(r"/remote\.php/.*")
            .unwrap()
            .find(url.path())
            .unwrap()
            .as_str()
            .to_string();
        return Err(tr::tr!(
            "Don't specify '{}' as part of the URL.",
            text_to_remove
        ));
    }
    Ok(())
}

pub fn validate_totp(code: &str) -> Result<(), String> {
    if code.chars().any(|c| !c.is_numeric()) {
        return Err(tr::tr!(
            "The provided 2FA code is invalid (should only contain digits)."
        ));
    }
    if code.len() != 6 {
        return Err(tr::tr!(
            "The provided 2FA code is invalid (should be 6 digits long)."
        ));
    }
    Ok(())
}

/// Check a login request the same way the old form did before submit.
pub fn validate(req: &LoginRequest) -> Result<(), String> {
    validate_name(&req.name)?;
    match req.kind.as_str() {
        "dropbox" | "gdrive" | "pcloud" => Ok(()),
        "nextcloud" | "owncloud" => {
            validate_url(&req.url, true)?;
            if req.username.is_empty() || req.password.is_empty() {
                return Err(tr::tr!("Enter a username and password."));
            }
            Ok(())
        }
        "webdav" => {
            validate_url(&req.url, false)?;
            if req.username.is_empty() || req.password.is_empty() {
                return Err(tr::tr!("Enter a username and password."));
            }
            Ok(())
        }
        "protondrive" => {
            if req.username.is_empty() || req.password.is_empty() {
                return Err(tr::tr!("Enter a username and password."));
            }
            if req.totp_enabled {
                validate_totp(&req.totp)?;
            }
            Ok(())
        }
        other => Err(format!("Unknown server type '{other}'.")),
    }
}
