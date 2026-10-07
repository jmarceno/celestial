//! Structs and functions for use with Rclone RPC calls.
use serde::Deserialize;
use serde_json::json;
use time::OffsetDateTime;

/// List configured remote names without reading credentials or assuming a provider.
pub fn get_remotes() -> Result<Vec<String>, String> {
    let response = librclone::rpc("config/listremotes", json!({}).to_string())?;
    #[derive(Deserialize)]
    struct Names {
        remotes: Vec<String>,
    }
    serde_json::from_str::<Names>(&response)
        .map(|names| names.remotes)
        .map_err(|err| err.to_string())
}

#[derive(Clone, Deserialize, Debug)]
pub struct RcloneError {
    pub error: String,
}

/// The output of an `operations/stat` command.
#[derive(Clone, Deserialize, Debug)]
pub struct RcloneStat {
    item: Option<RcloneRemoteItem>,
}

/// The output of an `operations/list` command.
#[derive(Clone, Deserialize, Debug)]
pub struct RcloneList {
    #[serde(rename = "list")]
    list: Vec<RcloneRemoteItem>,
}

/// The list of items in a folder, from the `list` object in the output of the
/// `operations/list` command.
#[derive(Clone, Deserialize, Debug)]
pub struct RcloneRemoteItem {
    #[serde(rename = "IsDir")]
    pub is_dir: bool,
    #[serde(rename = "Path")]
    pub path: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ModTime", with = "time::serde::rfc3339")]
    pub mod_time: OffsetDateTime,
}

/// The types of items to show in an `operations/list` command.
#[derive(Clone, Debug)]
pub enum RcloneListFilter {
    /// Return all items.
    All,
    /// Only return directories.
    Dirs,
}

/// Functions called synchronously on the synchronization worker.
pub mod sync {
    use super::{RcloneError, RcloneList, RcloneListFilter, RcloneRemoteItem, RcloneStat};
    use crate::util;
    use serde_json::json;

    /// Get a remote name.
    fn get_remote_name(remote: &str) -> String {
        if remote.ends_with(':') {
            panic!("Remote '{remote}' is not allowed to end with a ':'. Please omit it.",);
        }
        format!("{remote}:")
    }

    /// Run an Rclone command without blocking the GUI.
    fn run<T: ToString>(method: T, input: T) -> Result<String, String> {
        let method = method.to_string();
        let input = input.to_string();
        librclone::rpc(method, input)
    }

    /// Common function for some of the below command.
    fn common(command: &str, remote_name: &str, path: &str) -> Result<(), RcloneError> {
        let resp = run(
            command,
            &json!({
                "fs": get_remote_name(remote_name),
                "remote": util::strip_slashes(path),
            })
            .to_string(),
        );

        match resp {
            Ok(_) => Ok(()),
            Err(json_str) => Err(serde_json::from_str(&json_str).unwrap()),
        }
    }

    /// Delete a config.
    pub fn delete_config(remote_name: &str) -> Result<(), RcloneError> {
        let resp = run("config/delete", &json!({ "name": remote_name }).to_string());

        match resp {
            Ok(_) => Ok(()),
            Err(json_str) => Err(serde_json::from_str(&json_str).unwrap()),
        }
    }

    /// Get statistics about a file or folder.
    pub fn stat(remote_name: &str, path: &str) -> Result<Option<RcloneRemoteItem>, RcloneError> {
        let resp = run(
            "operations/stat",
            &json!({
                "fs": get_remote_name(remote_name),
                "remote": util::strip_slashes(path)
            })
            .to_string(),
        );

        match resp {
            Ok(json_str) => Ok(serde_json::from_str::<RcloneStat>(&json_str).unwrap().item),
            Err(json_str) => Err(serde_json::from_str(&json_str).unwrap()),
        }
    }

    /// List the files/folders in a path.
    pub fn list(
        remote_name: &str,
        path: &str,
        recursive: bool,
        filter: RcloneListFilter,
    ) -> Result<Vec<RcloneRemoteItem>, RcloneError> {
        let opts = match filter {
            RcloneListFilter::All => json!({ "recurse": recursive }),
            RcloneListFilter::Dirs => json!({"dirsOnly": true, "recurse": recursive}),
        };

        let resp = run(
            "operations/list",
            &json!({
                "fs": get_remote_name(remote_name),
                "remote": util::strip_slashes(path),
                "opt": opts
            })
            .to_string(),
        );

        match resp {
            Ok(json_str) => Ok(serde_json::from_str::<RcloneList>(&json_str).unwrap().list),
            Err(json_str) => Err(serde_json::from_str(&json_str).unwrap()),
        }
    }

    /// make a directory on the remote.
    pub fn mkdir(remote_name: &str, path: &str) -> Result<(), RcloneError> {
        common("operations/mkdir", remote_name, path)
    }

    /// Delete a file.
    pub fn delete(remote_name: &str, path: &str) -> Result<(), RcloneError> {
        common("operations/deletefile", remote_name, path)
    }
    /// Remove a directory and all of its contents.
    pub fn purge(remote_name: &str, path: &str) -> Result<(), RcloneError> {
        common("operations/purge", remote_name, path)
    }

    /// Utility for copy functions.
    fn copy(
        src_fs: &str,
        src_remote: &str,
        dst_fs: &str,
        dst_remote: &str,
    ) -> Result<(), RcloneError> {
        let resp = run(
            "operations/copyfile",
            &json!({
                "srcFs": src_fs,
                "srcRemote": util::strip_slashes(src_remote),
                "dstFs": dst_fs,
                "dstRemote": util::strip_slashes(dst_remote)
            })
            .to_string(),
        );

        match resp {
            Ok(_) => Ok(()),
            Err(json_str) => Err(serde_json::from_str(&json_str).unwrap()),
        }
    }

    /// Copy a file from the local machine to the remote.
    pub fn copy_to_remote(
        local_file: &str,
        remote_name: &str,
        remote_destination: &str,
    ) -> Result<(), RcloneError> {
        copy(
            "/",
            local_file,
            &get_remote_name(remote_name),
            remote_destination,
        )
    }

    /// Copy a file from the remote to the local machine.
    pub fn copy_to_local(
        local_destination: &str,
        remote_name: &str,
        remote_file: &str,
    ) -> Result<(), RcloneError> {
        copy(
            &get_remote_name(remote_name),
            remote_file,
            "/",
            local_destination,
        )
    }
}
