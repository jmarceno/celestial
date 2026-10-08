<p align="center">
  <img src="assets/com.unboundmark.Celestial-regular.svg" width="160" alt="Celestial logo: synchronization arrows around a star" />
</p>

<h1 align="center">Celestial</h1>

<p align="center">
  <strong>Your folders, in sync.</strong><br />
  Two-way cloud synchronization for Linux, with visible conflicts and recoverable changes.<br />
  Connect your cloud storage, choose a folder, and keep working.
</p>

<p align="center">
  <a href="#quick-start">Get started</a> ·
  <a href="#why-celestial">Why Celestial</a> ·
  <a href="#supported-providers">Providers</a> ·
  <a href="#how-synchronization-works">Sync behavior</a> ·
  <a href="#choose-what-to-sync">Exclusions</a> ·
  <a href="BUILDING.md">Build guide</a>
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0--or--later-2fb3a3" alt="GPL-3.0-or-later license" /></a>
  <img src="https://img.shields.io/badge/desktop-Linux-167d72" alt="Linux desktop" />
  <img src="https://img.shields.io/badge/cloud-7%20providers-167d72" alt="Seven supported cloud providers" />
</p>

---

## Why Celestial

Keep a working folder available locally and in your cloud account without
manually uploading every edit. Celestial brings folder pairing, sync status,
exclusions, and recovery into a Rust and Qt Quick desktop app powered by rclone.

| What you need | What Celestial gives you |
| --- | --- |
| **Your choice of storage** | Seven providers, including hosted cloud services, self-hosted Nextcloud and ownCloud, and generic WebDAV. |
| **Changes that travel both ways** | Local edits trigger synchronization within about a second. Remote changes are checked every 60 seconds, or sooner when a local edit starts a pass. |
| **Conflicts you can inspect** | Different edits on both sides keep both versions as visible conflict copies. |
| **A way back after sync changes** | Files Celestial replaces or deletes are preserved first, with a Restore action in the folder view. |
| **Control over what gets uploaded** | Editable defaults skip common caches and generated files. Add glob rules, regular expressions, or a rule that recognizes Rust build folders. |
| **Background synchronization** | Close the window and keep syncing from the system tray, with optional startup at login. |

## Supported providers

| Hosted cloud | Your own server |
| --- | --- |
| Google Drive · Dropbox · pCloud · Proton Drive | Nextcloud · ownCloud · WebDAV |

Account setup happens in the app. Provider-specific differences in change
detection and recovery are explained under [Provider behavior](#provider-behavior).

## Quick start

### 1. Get the portable app

Use a `Celestial-<version>-x86_64-portable.run` bundle, or build one following
[BUILDING.md](BUILDING.md). With the documented remote builder and packaging
dependencies configured:

```sh
bash scripts/build-run.sh
./scripts/build-portable.sh
./scripts/smoke-portable.sh build/portable/Celestial-*-x86_64-portable.run
```

The portable bundle includes Qt, QML, plugins, and shared libraries. Its
actual glibc requirement is printed during packaging; see the build guide
for compatibility details. Run it in a Linux graphical desktop session.

### 2. Install and launch

From the source checkout, install the newest bundle:

```sh
./scripts/install.sh
celestial
```

The installer puts the launcher at `~/.local/bin/celestial`; that directory
must be on your `PATH`. Add `--autostart` to the install command to start at
login. Use `celestial --background` to start directly in the tray.

You can also run a portable bundle directly after making it executable:

```sh
chmod +x Celestial-*-x86_64-portable.run
./Celestial-*-x86_64-portable.run
```

Its first start adds an app-menu entry. Keep the executable at a stable path.
`celestial uninstall` removes the menu entry, autostart entry, and icon while
keeping the executable, configuration, and synchronization history. Later
starts leave the menu entry removed; `celestial install` restores it.

### 3. Connect a folder

1. Choose **Add server**, select a provider, and sign in.
2. Select the account, choose **Add folder**, and pair a local folder with a remote folder.
3. Open the folder view to review **File/folder exclusions** and sync outcomes.
   Close the window when you're ready; synchronization continues in the tray.

Review exclusions before using your whole home directory as a sync root.
Deletions propagate in both directions; recovery preserves Celestial's own
replacements and deletions, rather than every edit made by other applications.

## How synchronization works

Celestial compares contents, not only timestamps. For each item it keeps the last contents both sides had in common, using the provider's hash (MD5, SHA-1, DropboxHash, ...) and, locally, size, modification time, inode change time and inode, so edits that keep the size and modification time (`cp -p`, `touch -d`) are still detected. Local files are hashed only when their metadata changed, on the first synchronization and when both sides changed.

| What changed | What happens |
| --- | --- |
| Only one side changed | That version is copied to the other side. |
| Both sides have the same contents | They converge without another copy. |
| Both sides have different contents | Both versions are kept as conflict copies on both sides. |
| One side deleted; the other is unchanged | The deletion propagates, after preserving the version Celestial removes. |
| One side deleted; the other changed | The changed version is restored and the outcome is listed. |

<details>
<summary>Conflict names, deletion outcomes, and folder handling</summary>

- One side changed: that version is copied to the other side.
- Both sides changed to the same contents, or the first synchronization finds the same contents on both sides: nothing is copied.
- Both sides changed differently, or the first synchronization finds different contents: both versions are kept. The version with the older modification time (the remote one on a tie) is renamed to `<name>.sync-conflict-<UTC date-time>-<local|remote>-<id>.<ext>`, and both files then exist on both sides. A file and a folder with the same name are handled the same way, renaming the file. At most 10 conflict copies are kept visible per item.
- Deleted on one side and unchanged on the other: the deletion is propagated. Deleted on one side but changed on the other: the changed version is restored and the outcome is listed; it can then be deleted everywhere from that entry.
- A folder is only removed when everything in it is being removed; changed, excluded or new contents keep it.

</details>

### Recovery and interrupted work

Every version Celestial replaces or deletes is preserved first. Locally it is moved into a hidden `.celestial-<id>` folder that Celestial creates inside the synced folder; on the remote it is moved to `.celestial-recovery/<pair>` at the root of the remote, which other clients of the account can see. Preserved versions are listed in the folder's view with a Restore action; restoring preserves the current version first. The newest 10 versions of each item are kept for 30 days; versions of unresolved conflicts, of unfinished operations, and the last version of a deleted item are not removed automatically. Stopping a folder or removing an account keeps them, and an internal folder left by a stopped folder is never synchronized. This protects against Celestial's own replacements and deletions; it is not a backup of arbitrary local edits, and an edit you overwrite yourself is not preserved.

Downloads are written to the internal folder and committed with an atomic rename; an edit that lands in the file at that moment is kept as a conflict copy. Uploads preserve the remote version before replacing it; an edit another client makes between Celestial's last check and that move is kept as a conflict copy. The embedded rclone offers no conditional writes, so an edit another client makes while an upload is in progress can still be replaced; that version then survives only in the provider's own version history, where one exists. Every change is journaled before it is made, so an interrupted operation is finished or undone on the next pass.

An item that cannot be synchronized right now is retried automatically after one minute, doubling up to one hour, while everything else continues. Renaming a synced file only in letter case is propagated on case-insensitive providers. Items that need you to act (other names that differ only by letter case on a case-insensitive provider, Unicode normalization variants, names that are not valid UTF-8, several remote items with one name, unfinished `.partial` downloads left by older versions) are listed with the reason, and nothing is changed until they are renamed. Only an unavailable or emptied folder, unreadable exclusions or a database failure pause a whole folder, and it is checked again automatically.

### Provider behavior

- Google Drive, Dropbox, pCloud, Proton Drive, Nextcloud and ownCloud report hashes, so remote edits are detected from contents.
- Generic WebDAV reports no hash and cannot set modification times. A remote edit that keeps the same size cannot be detected without downloading it. When both sides need comparing (a first synchronization, or both sides changed) and the sizes match, the remote version is downloaded and compared, so equal contents converge and different contents keep both. Uploads to providers without a hash are made from a snapshot, so the bytes sent are the bytes recorded.
- Providers without server-side moves preserve remote versions by copying and deleting, which uses transfer and quota.
- Deletions on Google Drive also go to the Drive trash; Celestial does not rely on provider trash.

## Choose what to sync

Each folder has an editable `.sync-exclude.lst`. Celestial creates a starting
list only when that file is absent; existing lists and removed rules stay as
you left them. Saving an empty list disables optional exclusions, while the
rules file and internal recovery folders remain protected.

| Rule type | Example | Use it for |
| --- | --- | --- |
| Relative glob | `*.tmp` | Matching temporary files, including nested files. |
| Home path | `~/go` | Excluding the entire Go directory at your home root. |
| Absolute path | `/run/user` | Excluding a specific local path. |
| Regular expression | `regex:(^\|/)target/(debug\|release)(/\|$)` | Matching generated subtrees relative to the sync folder. |
| Content-aware rule | `builtin:rust-target` | Excluding a `target` directory only when Rust build markers exist. |

Defaults cover common desktop caches, downloaded dependencies, and generated
outputs across many development tools. They preserve source files, lockfiles,
application state, and credentials in the documented locations. Installed
Steam games are excluded, including saves or mods inside those installation
folders unless you remove the rule.

See [Development exclusions](docs/default-exclusions.md) for language coverage
and the reasons behind each cache choice.

<details>
<summary>Full exclusion syntax, default behavior, and preserved data</summary>

When that file is absent, Celestial creates an editable starting list excluding common caches and temporary data: XDG caches, browser/Electron caches, Flatpak/Snap caches, Cargo downloads, Rust build folders (`builtin:rust-target`), the entire `~/go` directory, npm/Yarn/pnpm caches, Gradle caches, Python/tool caches, installed Steam games/caches/unfinished downloads, trash, and system runtime/temporary folders (`/run/user` and `/run/lock`, not removable drives under `/run/media`). The `~/go` rule is anchored to the home root, so other directories named `go` remain included. Existing lists are kept exactly as they are; saving an empty list disables every optional exclusion. Removed rules are never added back to an existing list.

Rules starting with `~/` refer to your home directory, `/` rules use absolute local paths, and other globs are relative to the selected folder. `*` in home/absolute rules covers one path component; `**` covers any depth. Relative globs retain their existing behavior (`*.tmp` also matches nested files). An excluded folder excludes its descendants on both sides. A selected folder that is itself inside an excluded location, such as `/tmp` or `~/.cache`, has all of its contents excluded. The rules file and Celestial's internal recovery folders remain protected.

Development defaults cover the TIOBE top 20 reviewed in September 2026 and additional languages found in the maintainer's projects: TypeScript, Kotlin, Elixir, D, Duende, Haskell, Lua, GDScript, CUDA, QML and Shell/Nix/PowerShell scripts. They include downloaded dependency caches, `node_modules`, generated framework files, native compiler caches, explicit .NET build profiles and language-specific outputs. Native object/bytecode extensions are excluded only inside `build` or `obj`; database journals, source files, lockfiles and generic build-folder documents remain included. See the [language coverage and cache choices](docs/default-exclusions.md).

The list also accepts `regex:` followed by a [Rust regular expression](https://docs.rs/regex/1.10.5/regex/#syntax), applied to paths relative to the selected folder and their parent paths. For example, `regex:(^|/)target/(debug|release)(/|$)` skips those build-profile subtrees at any depth. Regexes search for a match; use `^` and `$` to anchor a whole path. `glob:` explicitly selects glob syntax, including for literal filenames starting with `regex:` or `builtin:`. Invalid expressions are rejected before saving, and an invalid rules file pauses synchronization.

For excluding a whole Rust build folder based on its contents, use `builtin:rust-target` (included in new starting lists). It recognizes a directory named `target` containing a `debug` or `release` directory, or Cargo's `.rustc_info.json` file, and skips the entire folder on both sides. A plain `target` without those markers remains included. Detection uses both the local filesystem and the remote listing, so remote-only build folders are skipped too. Local probes never follow symbolic links. This rule follows [Cargo's build-cache layout](https://doc.rust-lang.org/cargo/reference/build-cache.html) and avoids requiring a regex to inspect filesystem contents.

The defaults preserve `.local/bin`, application data/state in `.local/share` and `.local/state`, browser bookmarks/passwords/extensions, Cargo configuration/credentials/installed binaries, encrypted files in `.Private`, and Steam's `userdata` and Proton `compatdata`. Installed Steam games (`/**/steamapps/common`) are excluded across all libraries because they can be downloaded again; saves or local mods stored inside that installation directory are also excluded unless you remove the rule. Whole `.local`, browser profiles, Steam installations, virtual environments, source repositories and generic `build` folders are not excluded: they can contain irreplaceable data. A `target` directory is excluded only when it has the Rust build markers described above. Review the list before using a whole home directory as a synchronization root. Cache choices follow the [XDG specification](https://specifications.freedesktop.org/basedir/0.8/), [Cargo's documented layout](https://doc.rust-lang.org/cargo/guide/cargo-home.html), [Chromium's cache layout](https://chromium.googlesource.com/chromium/src/+/main/docs/user_data_dir.md), [Firefox profiles](https://firefox-source-docs.mozilla.org/toolkit/profile/index.html), and [Gradle's cache layout](https://docs.gradle.org/current/userguide/directory_layout.html).

</details>

## Configuration and upgrades

Configuration and synchronization history remain in `$XDG_CONFIG_HOME/celestial` (normally `~/.config/celestial`): `rclone.conf` and `celestial.db`. Keep both when backing up or moving the application.

The app was previously called Celeste. On first start, Celestial moves `~/.config/celeste` and `celeste.db` to the new names, and `celestial install` (or starting the portable bundle) replaces the old menu and autostart entries. `scripts/install.sh` also removes the old `~/.local/bin/celeste` launcher.

Local folders are watched for changes, so local edits sync within about a second without rescanning idle folders. Remote folders are checked every 60 seconds, or sooner when a local change starts a pass.

## Development and validation

See [BUILDING.md](BUILDING.md) for remote release builds, packaging, and smoke
tests. The [synchronization validation guide](docs/synchronization-validation.md)
describes disposable test remotes, recovery drills, and provider limitations.
The [privacy policy](PRIVACY.md) covers application data collection.

## License and origin

Celestial is licensed under GPL-3.0-or-later. This Qt Quick application is derived from [Hunter Wittenborn's Celeste](https://github.com/hwittenborn/celeste); the original copyright and license are retained.

Found a sync issue or have a feature idea? [Open an issue](https://gitea.home.unboundmark.com/jmarceno/celestial/issues)
with the provider, what changed on each side, and the outcome you expected.
