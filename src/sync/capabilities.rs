//! What each remote can do, from `operations/fsinfo` on the embedded rclone.
//!
//! The embedded rclone exposes no conditional writes and no change
//! notification, so neither is recorded here.

use super::store;
use crate::rclone::{self, FsInfo, MOD_TIME_NOT_SUPPORTED};
use sea_orm::DatabaseConnection;
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub hashes: Vec<String>,
    /// Modification time precision in nanoseconds.
    pub precision_ns: i64,
    pub case_insensitive: bool,
    pub duplicate_files: bool,
    pub partial_uploads: bool,
    pub server_move: bool,
    pub server_copy: bool,
    pub empty_dirs: bool,
    pub set_modtime: bool,
    pub list_r: bool,
    /// What a deletion does on the provider, when known. Provider trash is
    /// never Celestial's recovery guarantee.
    pub trash: Option<String>,
}

/// Hash types in order of preference: cheap and widely supported first.
const PREFERRED: [&str; 6] = ["md5", "sha1", "quickxor", "dropbox", "sha256", "crc32"];

impl Capabilities {
    pub fn from_fsinfo(info: &FsInfo, remote_type: Option<&str>) -> Self {
        let feature = |name: &str| info.features.get(name).copied().unwrap_or(false);
        Self {
            hashes: info.hashes.clone(),
            precision_ns: info.precision.max(1),
            case_insensitive: feature("CaseInsensitive"),
            duplicate_files: feature("DuplicateFiles"),
            partial_uploads: feature("PartialUploads"),
            server_move: feature("Move"),
            server_copy: feature("Copy"),
            empty_dirs: feature("CanHaveEmptyDirectories"),
            set_modtime: info.precision < MOD_TIME_NOT_SUPPORTED,
            list_r: feature("ListR"),
            trash: remote_type.and_then(trash_behavior).map(str::to_owned),
        }
    }

    /// The hash type compared for this remote, if it has any.
    pub fn preferred_hash(&self) -> Option<&str> {
        PREFERRED
            .iter()
            .copied()
            .find(|kind| self.hashes.iter().any(|hash| hash == kind))
            .or_else(|| {
                self.hashes
                    .iter()
                    .map(String::as_str)
                    .find(|hash| *hash != "none")
            })
    }
}

fn trash_behavior(kind: &str) -> Option<&'static str> {
    match kind {
        "drive" => Some("Deleting a file moves it to the Google Drive trash."),
        "onedrive" => Some("Deleting a file moves it to the OneDrive recycle bin."),
        "dropbox" => Some("Dropbox keeps deleted files for its version history period."),
        "pcloud" => Some("Deleting a file moves it to the pCloud trash."),
        _ => None,
    }
}

fn remote_type(remote_name: &str) -> Option<String> {
    let reply = librclone::rpc("config/get", json!({ "name": remote_name }).to_string()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&reply).ok()?;
    value.get("type")?.as_str().map(str::to_owned)
}

/// Read the capabilities from rclone and record them for the remote.
pub fn probe(
    db: &DatabaseConnection,
    remote_name: &str,
    rclone_version: &str,
) -> Result<Capabilities, String> {
    let info = rclone::sync::fsinfo(remote_name).map_err(|err| err.error)?;
    let caps = Capabilities::from_fsinfo(&info, remote_type(remote_name).as_deref());
    store::save_capabilities(db, remote_name, &caps, rclone_version)
        .map_err(|err| format!("Could not record the remote's capabilities: {err}"))?;
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_map_to_capabilities() {
        let info = FsInfo {
            hashes: vec!["sha1".into(), "md5".into()],
            precision: 1_000_000_000,
            features: [
                ("CaseInsensitive".to_owned(), true),
                ("Move".to_owned(), true),
                ("DuplicateFiles".to_owned(), false),
            ]
            .into(),
        };
        let caps = Capabilities::from_fsinfo(&info, Some("drive"));
        assert!(caps.case_insensitive && caps.server_move && !caps.duplicate_files);
        assert!(caps.set_modtime);
        assert_eq!(caps.preferred_hash(), Some("md5"));
        assert!(caps.trash.is_some());
        let webdav = Capabilities::from_fsinfo(
            &FsInfo {
                hashes: vec![],
                precision: MOD_TIME_NOT_SUPPORTED,
                features: Default::default(),
            },
            Some("webdav"),
        );
        assert_eq!(webdav.preferred_hash(), None);
        assert!(!webdav.set_modtime);
    }
}
