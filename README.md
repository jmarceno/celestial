# Celestial

Celestial synchronizes folders in both directions between your computer and cloud storage, using Rust, Qt Quick and rclone. A system tray icon keeps synchronization running when the window is closed.

Supported providers: Dropbox, Google Drive, Nextcloud, Owncloud, pCloud, Proton Drive and WebDAV.

Exclusions are stored in `.sync-exclude.lst` in each local folder.

When that file is absent, Celestial creates an editable starting list excluding common caches and temporary data: XDG caches, browser/Electron caches, Flatpak/Snap caches, Cargo downloads, npm/Yarn/pnpm caches, Gradle caches, Python/tool caches, installed Steam games/caches/unfinished downloads, trash, and system runtime/temporary folders. Existing lists are kept exactly as they are; saving an empty list disables every optional exclusion. Removed rules are never added back to an existing list.

Rules starting with `~/` refer to your home directory, `/` rules use absolute local paths, and other globs are relative to the selected folder. `*` in home/absolute rules covers one path component; `**` covers any depth. Relative globs retain their existing behavior (`*.tmp` also matches nested files). An excluded folder excludes its descendants on both sides. The rules file and Celestial's internal recovery folders remain protected.

The defaults preserve `.local/bin`, application data/state in `.local/share` and `.local/state`, browser bookmarks/passwords/extensions, Cargo configuration/credentials/installed binaries, encrypted files in `.Private`, and Steam's `userdata` and Proton `compatdata`. Installed Steam games (`/**/steamapps/common`) are excluded across all libraries because they can be downloaded again; saves or local mods stored inside that installation directory are also excluded unless you remove the rule. Whole `.local`, browser profiles, Steam installations, virtual environments, source repositories and generic `build`/`target` folders are not excluded: they can contain irreplaceable data. Review the list before using a whole home directory as a synchronization root. Cache choices follow the [XDG specification](https://specifications.freedesktop.org/basedir/0.8/), [Cargo's documented layout](https://doc.rust-lang.org/cargo/guide/cargo-home.html), [Chromium's cache layout](https://chromium.googlesource.com/chromium/src/+/main/docs/user_data_dir.md), [Firefox profiles](https://firefox-source-docs.mozilla.org/toolkit/profile/index.html), and [Gradle's cache layout](https://docs.gradle.org/current/userguide/directory_layout.html).

## Installation

Build the portable executable following [BUILDING.md](BUILDING.md), then install it:

```sh
./scripts/install.sh build/portable/Celestial-0.8.3-x86_64-portable.run
```

Running the portable `.run` directly also adds the app-menu entry the first time it starts. `celestial uninstall` removes the menu entry, autostart entry and icon, and keeps the executable, configuration and synchronization history; the menu entry is not added back on later starts, but `celestial install` restores it.

Add `--autostart` to start Celestial at login. Launch `celestial` to open the window or `celestial --background` to start in the tray.

## Configuration

Configuration and synchronization history remain in `$XDG_CONFIG_HOME/celestial` (normally `~/.config/celestial`): `rclone.conf` and `celestial.db`. Keep both when backing up or moving the application.

The app was previously called Celeste. On first start, Celestial moves `~/.config/celeste` and `celeste.db` to the new names, and `celestial install` (or starting the portable bundle) replaces the old menu and autostart entries. `scripts/install.sh` also removes the old `~/.local/bin/celeste` launcher.

Local folders are watched for changes, so local edits sync within about a second without rescanning idle folders. Remote folders are checked every 60 seconds, or sooner when a local change starts a pass.

## How synchronization works

Celestial compares contents, not only timestamps. For each item it keeps the last contents both sides had in common, using the provider's hash (MD5, SHA-1, DropboxHash, ...) and, locally, size, modification time, inode change time and inode, so edits that keep the size and modification time (`cp -p`, `touch -d`) are still detected. Local files are hashed only when their metadata changed, on the first synchronization and when both sides changed.

- One side changed: that version is copied to the other side.
- Both sides changed to the same contents, or the first synchronization finds the same contents on both sides: nothing is copied.
- Both sides changed differently, or the first synchronization finds different contents: both versions are kept. The version with the older modification time (the remote one on a tie) is renamed to `<name>.sync-conflict-<UTC date-time>-<local|remote>-<id>.<ext>`, and both files then exist on both sides. A file and a folder with the same name are handled the same way, renaming the file. At most 10 conflict copies are kept visible per item.
- Deleted on one side and unchanged on the other: the deletion is propagated. Deleted on one side but changed on the other: the changed version is restored and the outcome is listed; it can then be deleted everywhere from that entry.
- A folder is only removed when everything in it is being removed; changed, excluded or new contents keep it.

Every version Celestial replaces or deletes is preserved first. Locally it is moved into a hidden `.celestial-<id>` folder that Celestial creates inside the synced folder; on the remote it is moved to `.celestial-recovery/<pair>` at the root of the remote, which other clients of the account can see. Preserved versions are listed in the folder's view with a Restore action; restoring preserves the current version first. The newest 10 versions of each item are kept for 30 days; versions of unresolved conflicts, of unfinished operations, and the last version of a deleted item are not removed automatically. Stopping a folder or removing an account keeps them, and an internal folder left by a stopped folder is never synchronized. This protects against Celestial's own replacements and deletions; it is not a backup of arbitrary local edits, and an edit you overwrite yourself is not preserved.

Downloads are written to the internal folder and committed with an atomic rename; an edit that lands in the file at that moment is kept as a conflict copy. Uploads preserve the remote version before replacing it; an edit another client makes between Celestial's last check and that move is kept as a conflict copy. The embedded rclone offers no conditional writes, so an edit another client makes while an upload is in progress can still be replaced; that version then survives only in the provider's own version history, where one exists. Every change is journaled before it is made, so an interrupted operation is finished or undone on the next pass.

An item that cannot be synchronized right now is retried automatically after one minute, doubling up to one hour, while everything else continues. Renaming a synced file only in letter case is propagated on case-insensitive providers. Items that need you to act (other names that differ only by letter case on a case-insensitive provider, Unicode normalization variants, names that are not valid UTF-8, several remote items with one name, unfinished `.partial` downloads left by older versions) are listed with the reason, and nothing is changed until they are renamed. Only an unavailable or emptied folder, unreadable exclusions or a database failure pause a whole folder, and it is checked again automatically.

Provider differences:

- Google Drive, Dropbox, pCloud, Proton Drive, Nextcloud and ownCloud report hashes, so remote edits are detected from contents.
- Generic WebDAV reports no hash and cannot set modification times. A remote edit that keeps the same size cannot be detected without downloading it. When both sides need comparing (a first synchronization, or both sides changed) and the sizes match, the remote version is downloaded and compared, so equal contents converge and different contents keep both. Uploads to providers without a hash are made from a snapshot, so the bytes sent are the bytes recorded.
- Providers without server-side moves preserve remote versions by copying and deleting, which uses transfer and quota.
- Deletions on Google Drive also go to the Drive trash; Celestial does not rely on provider trash.

## License and origin

Celestial is licensed under GPL-3.0-or-later. This Qt Quick application is derived from [Hunter Wittenborn's Celeste](https://github.com/hwittenborn/celeste); the original copyright and license are retained.
