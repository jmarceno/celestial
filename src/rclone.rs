//! Structs and functions for use with Rclone RPC calls.
//!
//! Every response is decoded fallibly: a malformed reply or an invalid remote
//! name is an ordinary error, never a panic and never an empty observation.
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// List configured remote names without reading credentials or assuming a provider.
pub fn get_remotes() -> Result<Vec<String>, String> {
    let response = librclone::rpc("config/listremotes", json!({}).to_string())?;
    #[derive(Deserialize)]
    struct Names {
        remotes: Option<Vec<String>>,
    }
    serde_json::from_str::<Names>(&response)
        .map(|names| names.remotes.unwrap_or_default())
        .map_err(|err| err.to_string())
}

#[derive(Clone, Deserialize, Debug, PartialEq, Eq)]
pub struct RcloneError {
    pub error: String,
}

impl RcloneError {
    pub fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }

    /// Decode an error reply, keeping the raw text when it is not the
    /// expected JSON object.
    fn from_reply(text: &str) -> Self {
        serde_json::from_str::<Self>(text).unwrap_or_else(|_| Self::new(text))
    }
}

impl std::fmt::Display for RcloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

/// The output of an `operations/stat` command.
#[derive(Clone, Deserialize, Debug)]
struct RcloneStat {
    item: Option<RcloneRemoteItem>,
}

/// The output of an `operations/list` command.
#[derive(Clone, Deserialize, Debug)]
struct RcloneList {
    #[serde(rename = "list")]
    list: Vec<RcloneRemoteItem>,
}

/// One item from `operations/list` or `operations/stat`.
#[derive(Clone, Deserialize, Debug, PartialEq, Eq)]
pub struct RcloneRemoteItem {
    #[serde(rename = "IsDir")]
    pub is_dir: bool,
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ModTime", with = "time::serde::rfc3339")]
    pub mod_time: OffsetDateTime,
    /// Size in bytes; `-1` for folders and unknown sizes.
    #[serde(rename = "Size", default = "unknown_size")]
    pub size: i64,
    /// Content hashes by rclone hash name (`md5`, `sha1`, `quickxor`, ...).
    #[serde(rename = "Hashes", default)]
    pub hashes: BTreeMap<String, String>,
    /// Object identity on backends that report one. It identifies an
    /// object, not a version of its contents.
    #[serde(rename = "ID", default)]
    pub id: Option<String>,
}

fn unknown_size() -> i64 {
    -1
}

/// The types of items to show in an `operations/list` command.
#[derive(Clone, Debug)]
pub enum RcloneListFilter {
    /// Return all items.
    All,
    /// Only return directories.
    Dirs,
}

/// Options for listings and stats beyond the filter.
#[derive(Clone, Debug, Default)]
pub struct ListOptions {
    /// Hash types to request; empty requests none.
    pub hash_types: Vec<String>,
    /// Ask the backend for one recursive listing where it supports `ListR`.
    pub use_list_r: bool,
}

/// What `operations/fsinfo` reports about a remote.
#[derive(Clone, Deserialize, Debug, Default, PartialEq, Eq)]
pub struct FsInfo {
    #[serde(rename = "Hashes", default)]
    pub hashes: Vec<String>,
    /// Modification time precision in nanoseconds.
    #[serde(rename = "Precision", default)]
    pub precision: i64,
    #[serde(rename = "Features", default)]
    pub features: BTreeMap<String, bool>,
}

/// rclone's `fs.ModTimeNotSupported`: 100 years, in nanoseconds.
pub const MOD_TIME_NOT_SUPPORTED: i64 = 100 * 365 * 24 * 3600 * 1_000_000_000;

/// The embedded rclone version from `core/version`.
pub fn version() -> Result<String, RcloneError> {
    let reply =
        librclone::rpc("core/version", "{}").map_err(|text| RcloneError::from_reply(&text))?;
    let value: Value =
        serde_json::from_str(&reply).map_err(|err| RcloneError::new(err.to_string()))?;
    value
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| RcloneError::new("core/version returned no version"))
}

/// What `core/stats` reports about one stats group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GroupStats {
    /// Bytes transferred since the group started, including files still
    /// being transferred.
    pub bytes: u64,
    /// Bytes of the files being transferred now.
    pub in_flight: u64,
}

impl GroupStats {
    fn from_reply(reply: &str) -> Result<Self, RcloneError> {
        let value: Value = serde_json::from_str(reply)
            .map_err(|err| RcloneError::new(format!("Malformed reply from core/stats: {err}")))?;
        let bytes = |value: &Value| value.get("bytes").and_then(Value::as_u64).unwrap_or(0);
        Ok(Self {
            bytes: bytes(&value),
            in_flight: value
                .get("transferring")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(bytes).sum())
                .unwrap_or(0),
        })
    }
}

/// Read a stats group. Safe to call from any thread while a transfer in that
/// group runs; not counted in [`sync::RPC_CALLS`].
pub fn group_stats(group: &str) -> Result<GroupStats, RcloneError> {
    let reply = librclone::rpc("core/stats", json!({ "group": group }).to_string())
        .map_err(|text| RcloneError::from_reply(&text))?;
    GroupStats::from_reply(&reply)
}

/// Forget a stats group and its counters.
pub fn delete_group_stats(group: &str) {
    if let Err(text) = librclone::rpc("core/stats-delete", json!({ "group": group }).to_string()) {
        tracing::debug!("could not delete stats group {group}: {text}");
    }
}

/// What `operations/size` reports about a folder.
#[derive(Clone, Copy, Deserialize, Debug, Default, PartialEq, Eq)]
pub struct FolderSize {
    pub count: u64,
    pub bytes: u64,
    /// Objects whose size the remote does not report.
    #[serde(default)]
    pub sizeless: u64,
}

/// Functions called synchronously on the synchronization worker.
pub mod sync {
    use super::{
        FolderSize, FsInfo, ListOptions, RcloneError, RcloneList, RcloneListFilter,
        RcloneRemoteItem, RcloneStat,
    };
    use crate::util;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// RPCs made by this process, for measurement.
    pub static RPC_CALLS: AtomicU64 = AtomicU64::new(0);

    thread_local! {
        /// The rclone stats group transfers on this thread are counted in.
        static STATS_GROUP: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Count the transfers this thread makes in `group` (`None` stops), so
    /// another thread can read their progress with [`super::group_stats`].
    pub fn set_stats_group(group: Option<String>) {
        STATS_GROUP.with(|current| *current.borrow_mut() = group);
    }

    /// The filesystem string for a configured remote.
    pub fn remote_fs(remote: &str) -> Result<String, RcloneError> {
        if remote.is_empty() || remote.contains(':') || remote.contains('/') {
            return Err(RcloneError::new(format!(
                "'{remote}' is not a valid remote name"
            )));
        }
        Ok(format!("{remote}:"))
    }

    /// Run an Rclone command and return the raw reply.
    fn run(method: &str, input: Value) -> Result<String, RcloneError> {
        RPC_CALLS.fetch_add(1, Ordering::Relaxed);
        librclone::rpc(method, input.to_string()).map_err(|text| RcloneError::from_reply(&text))
    }

    fn decode<T: DeserializeOwned>(method: &str, reply: &str) -> Result<T, RcloneError> {
        serde_json::from_str(reply)
            .map_err(|err| RcloneError::new(format!("Malformed reply from {method}: {err}")))
    }

    fn on_remote(method: &str, remote_name: &str, path: &str) -> Result<(), RcloneError> {
        run(
            method,
            json!({
                "fs": remote_fs(remote_name)?,
                "remote": util::strip_slashes(path),
            }),
        )
        .map(|_| ())
    }

    /// Delete a config.
    pub fn delete_config(remote_name: &str) -> Result<(), RcloneError> {
        run("config/delete", json!({ "name": remote_name })).map(|_| ())
    }

    fn list_opt(recursive: bool, filter: &RcloneListFilter, options: &ListOptions) -> Value {
        let mut opt = json!({ "recurse": recursive });
        if matches!(filter, RcloneListFilter::Dirs) {
            opt["dirsOnly"] = json!(true);
        }
        if !options.hash_types.is_empty() {
            opt["showHash"] = json!(true);
            opt["hashTypes"] = json!(options.hash_types);
        }
        opt
    }

    /// Get statistics about a file or folder.
    pub fn stat(remote_name: &str, path: &str) -> Result<Option<RcloneRemoteItem>, RcloneError> {
        stat_with(remote_name, path, &ListOptions::default())
    }

    /// Get statistics about a file or folder, with the requested hashes.
    pub fn stat_with(
        remote_name: &str,
        path: &str,
        options: &ListOptions,
    ) -> Result<Option<RcloneRemoteItem>, RcloneError> {
        let reply = run(
            "operations/stat",
            json!({
                "fs": remote_fs(remote_name)?,
                "remote": util::strip_slashes(path),
                "opt": list_opt(false, &RcloneListFilter::All, options),
            }),
        )?;
        Ok(decode::<RcloneStat>("operations/stat", &reply)?.item)
    }

    /// Hash a local file with one of rclone's hash types. `None` means the
    /// type could not be computed.
    pub fn local_hash(path: &str, hash_type: &str) -> Result<Option<String>, RcloneError> {
        let reply = run(
            "operations/stat",
            json!({
                "fs": "/",
                "remote": util::strip_slashes(path),
                "opt": { "showHash": true, "hashTypes": [hash_type] },
            }),
        )?;
        let item = decode::<RcloneStat>("operations/stat", &reply)?
            .item
            .ok_or_else(|| RcloneError::new(format!("'{path}' disappeared while hashing")))?;
        Ok(item
            .hashes
            .get(hash_type)
            .cloned()
            .filter(|hash| !hash.is_empty()))
    }

    /// List the files/folders in a path.
    pub fn list(
        remote_name: &str,
        path: &str,
        recursive: bool,
        filter: RcloneListFilter,
    ) -> Result<Vec<RcloneRemoteItem>, RcloneError> {
        list_with(
            remote_name,
            path,
            recursive,
            filter,
            &ListOptions::default(),
        )
    }

    /// List the files/folders in a path, with hashes and listing options.
    pub fn list_with(
        remote_name: &str,
        path: &str,
        recursive: bool,
        filter: RcloneListFilter,
        options: &ListOptions,
    ) -> Result<Vec<RcloneRemoteItem>, RcloneError> {
        let mut input = json!({
            "fs": remote_fs(remote_name)?,
            "remote": util::strip_slashes(path),
            "opt": list_opt(recursive, &filter, options),
        });
        if options.use_list_r {
            input["_config"] = json!({ "UseListR": true });
        }
        let reply = run("operations/list", input)?;
        Ok(decode::<RcloneList>("operations/list", &reply)?.list)
    }

    /// Features, hashes and precision of a remote.
    pub fn fsinfo(remote_name: &str) -> Result<FsInfo, RcloneError> {
        let reply = run(
            "operations/fsinfo",
            json!({ "fs": remote_fs(remote_name)? }),
        )?;
        decode("operations/fsinfo", &reply)
    }

    /// Free space reported by the remote, when it reports any.
    pub fn free_space(remote_name: &str) -> Result<Option<i64>, RcloneError> {
        let reply = run("operations/about", json!({ "fs": remote_fs(remote_name)? }))?;
        let value: Value = decode("operations/about", &reply)?;
        Ok(value.get("free").and_then(Value::as_i64))
    }

    /// Files and bytes under a remote folder, recursively. A folder that does
    /// not exist yet holds nothing.
    pub fn folder_size(remote_name: &str, path: &str) -> Result<FolderSize, RcloneError> {
        let fs = format!("{}{}", remote_fs(remote_name)?, util::strip_slashes(path));
        let reply = match run("operations/size", json!({ "fs": fs })) {
            Ok(reply) => reply,
            Err(err) if err.error.contains("directory not found") => {
                return Ok(FolderSize::default());
            }
            Err(err) => return Err(err),
        };
        decode("operations/size", &reply)
    }

    /// make a directory on the remote.
    pub fn mkdir(remote_name: &str, path: &str) -> Result<(), RcloneError> {
        on_remote("operations/mkdir", remote_name, path)
    }

    /// Remove an empty directory on the remote; fails when it has contents.
    pub fn rmdir(remote_name: &str, path: &str) -> Result<(), RcloneError> {
        on_remote("operations/rmdir", remote_name, path)
    }

    /// Delete a file.
    pub fn delete(remote_name: &str, path: &str) -> Result<(), RcloneError> {
        on_remote("operations/deletefile", remote_name, path)
    }

    /// Copy one file. Every intended transfer sets `IgnoreTimes`: otherwise
    /// rclone skips a destination whose size and modification time already
    /// match and reports success without copying.
    fn transfer(
        method: &str,
        src_fs: &str,
        src_remote: &str,
        dst_fs: &str,
        dst_remote: &str,
    ) -> Result<(), RcloneError> {
        let mut input = json!({
            "srcFs": src_fs,
            "srcRemote": util::strip_slashes(src_remote),
            "dstFs": dst_fs,
            "dstRemote": util::strip_slashes(dst_remote),
            "_config": { "IgnoreTimes": true },
        });
        if let Some(group) = STATS_GROUP.with(|group| group.borrow().clone()) {
            input["_group"] = json!(group);
        }
        run(method, input).map(|_| ())
    }

    /// Copy a file from the local machine to the remote.
    pub fn copy_to_remote(
        local_file: &str,
        remote_name: &str,
        remote_destination: &str,
    ) -> Result<(), RcloneError> {
        transfer(
            "operations/copyfile",
            "/",
            local_file,
            &remote_fs(remote_name)?,
            remote_destination,
        )
    }

    /// Copy a file from the remote to the local machine.
    pub fn copy_to_local(
        local_destination: &str,
        remote_name: &str,
        remote_file: &str,
    ) -> Result<(), RcloneError> {
        transfer(
            "operations/copyfile",
            &remote_fs(remote_name)?,
            remote_file,
            "/",
            local_destination,
        )
    }

    /// Copy a file between two paths of the same remote.
    pub fn copy_on_remote(remote_name: &str, from: &str, to: &str) -> Result<(), RcloneError> {
        let fs = remote_fs(remote_name)?;
        transfer("operations/copyfile", &fs, from, &fs, to)
    }

    /// Move a file between two paths of the same remote (server-side where
    /// the backend supports it, otherwise copy and delete).
    pub fn move_on_remote(remote_name: &str, from: &str, to: &str) -> Result<(), RcloneError> {
        let fs = remote_fs(remote_name)?;
        transfer("operations/movefile", &fs, from, &fs, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_with_separators_are_errors_not_panics() {
        assert!(sync::remote_fs("drive:").is_err());
        assert!(sync::remote_fs("").is_err());
        assert!(sync::remote_fs("a/b").is_err());
        assert_eq!(sync::remote_fs("drive").unwrap(), "drive:");
    }

    #[test]
    fn group_stats_add_up_the_files_in_flight() {
        let reply = r#"{"bytes": 1500, "speed": 10.5, "transferring": [
            {"name": "a", "bytes": 200, "size": 1000},
            {"name": "b", "bytes": 300, "size": 4000}
        ]}"#;
        assert_eq!(
            GroupStats::from_reply(reply).unwrap(),
            GroupStats {
                bytes: 1500,
                in_flight: 500
            }
        );
        assert_eq!(
            GroupStats::from_reply(r#"{"bytes": 7}"#).unwrap(),
            GroupStats {
                bytes: 7,
                in_flight: 0
            }
        );
        assert!(GroupStats::from_reply("nope").is_err());
    }

    #[test]
    fn malformed_error_replies_keep_their_text() {
        assert_eq!(RcloneError::from_reply("not json").error, "not json");
        assert_eq!(
            RcloneError::from_reply(r#"{"error":"boom","status":500}"#).error,
            "boom"
        );
    }

    #[test]
    fn items_keep_size_hashes_id_and_nanoseconds() {
        let item: RcloneRemoteItem = serde_json::from_str(
            r#"{"Path":"a/b","Name":"b","Size":5,"ModTime":"1969-12-31T23:59:59.123456789Z","IsDir":false,"Hashes":{"md5":"x"},"ID":"id1"}"#,
        )
        .unwrap();
        assert_eq!(item.size, 5);
        assert_eq!(item.hashes["md5"], "x");
        assert_eq!(item.id.as_deref(), Some("id1"));
        assert_eq!(item.mod_time.unix_timestamp(), -1);
        assert_eq!(item.mod_time.nanosecond(), 123_456_789);
    }
}
