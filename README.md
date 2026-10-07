# Celestial

Celestial synchronizes folders in both directions between your computer and cloud storage, using Rust, Qt Quick and rclone. A system tray icon keeps synchronization running when the window is closed.

Supported providers: Dropbox, Google Drive, Nextcloud, Owncloud, pCloud, Proton Drive and WebDAV.

The app reports conflicts when both copies have changed and supports exclusions stored in `.sync-exclude.lst` in each local folder.

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

Local folders are watched for changes, so local edits sync within about a second without rescanning idle folders. Remote folders are checked every 60 seconds, or sooner when a local change starts a pass. See [the synchronization audit](docs/synchronization-audit.md) for the monitoring implementation and remaining limitations.

## License and origin

Celestial is licensed under GPL-3.0-or-later. This Qt Quick application is derived from [Hunter Wittenborn's Celeste](https://github.com/hwittenborn/celeste); the original copyright and license are retained.
