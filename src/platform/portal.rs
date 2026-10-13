//! The desktop's own folder chooser, through the XDG desktop portal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// What the person did in the chooser.
pub enum Picked {
    Folder(PathBuf),
    Cancelled,
}

/// Show the system folder chooser and block until it closes.
///
/// An error means no portal answered; the caller should fall back to
/// another chooser.
pub fn pick_folder(title: &str, start: Option<&Path>) -> Result<Picked, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let conn = Connection::session().map_err(|err| err.to_string())?;
    let sender = conn
        .unique_name()
        .ok_or("no D-Bus name")?
        .trim_start_matches(':')
        .replace('.', "_");
    let token = format!(
        "celestial_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let request_path = format!("{PATH}/request/{sender}/{token}");

    // Subscribe before the call so a fast reply is not missed.
    let request = Proxy::new(
        &conn,
        DESTINATION,
        request_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .map_err(|err| err.to_string())?;
    let mut responses = request
        .receive_signal("Response")
        .map_err(|err| err.to_string())?;

    let chooser = Proxy::new(
        &conn,
        DESTINATION,
        PATH,
        "org.freedesktop.portal.FileChooser",
    )
    .map_err(|err| err.to_string())?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("directory", Value::from(true));
    options.insert("modal", Value::from(true));
    if let Some(start) = start.filter(|path| path.is_dir()) {
        let mut bytes = start.as_os_str().as_encoded_bytes().to_vec();
        bytes.push(0);
        options.insert("current_folder", Value::from(bytes));
    }
    let _: zbus::zvariant::OwnedObjectPath = chooser
        .call("OpenFile", &("", title, options))
        .map_err(|err| err.to_string())?;

    let message = responses.next().ok_or("the portal closed the request")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message
        .body()
        .deserialize()
        .map_err(|err| err.to_string())?;
    if code != 0 {
        return Ok(Picked::Cancelled);
    }
    let uris: Vec<String> = results
        .get("uris")
        .ok_or("the portal returned no folder")?
        .try_clone()
        .map_err(|err| err.to_string())?
        .try_into()
        .map_err(|err: zbus::zvariant::Error| err.to_string())?;
    let uri = uris.first().ok_or("the portal returned no folder")?;
    let path = url::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .ok_or_else(|| format!("not a local folder: {uri}"))?;
    Ok(Picked::Folder(path))
}
