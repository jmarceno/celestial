# Building Celestial

Rust compilation, tests and Clippy run on the configured `rust-builder` through `cargo remote`. Formatting stays local. The app uses Qt Quick 6 and librclone; no GTK build tooling is needed.

The remote configuration is `~/.config/cargo-remote/cargo-remote.toml`. SSH connection settings are in `~/.ssh/config`. Include dotfiles with `-h` to transfer `.cargo/config.toml`, which selects the linker required by cxx-qt.

```sh
cargo fmt --check
cargo remote -h -- test --locked
cargo remote -h -- clippy --locked --all-targets
bash scripts/build-run.sh
```

`Cargo.toml` denies Rust warnings and the configured Clippy groups. The installed cargo-remote 0.2.0 strips an additional `--` separator, so forwarding `-- -D warnings` fails with `unexpected argument '-D'`; use the manifest lint settings as above.

`scripts/build-run.sh` uses the `cargo remote-release` alias from `.cargo/config.toml` and produces `target/release/celestial.run`, retaining `target/release/celestial` for packaging. Stable Cargo does not support dots in binary target names, so the script adds the suffix after a successful build. This `.run` file is the native executable; the portable bundle is generated separately below. Dependencies and intermediate build files stay on the builder. If the remote build fails, report the error instead of compiling locally.

## Tests

Synchronization tests run against disposable local folders behind rclone `alias`, case-insensitive `local` and `memory` remotes; they never use a cloud account. Failpoints in the sync engine (`src/sync/failpoint.rs`) interrupt an operation at each step, as a crash would, or inject an edit inside a commit window; the tests then check the journal replay and that every version survived.

The 20,000-file measurement is ignored by default. Run it on the builder, in the folder `cargo remote` synchronizes to:

```sh
ssh rust-builder 'cd ~/remote-builds/<project hash> && source ~/.profile && cargo test --lib measure_twenty -- --ignored --nocapture'
```

## Staging and portable packaging

```sh
./scripts/build-native.sh --release
./scripts/build-portable.sh
./scripts/smoke-portable.sh build/portable/Celestial-0.8.3-x86_64-portable.run
```

The staging script builds remotely and copies the executable, desktop entry, icon and license into `build/stage`. Packaging bundles the executable with local Qt plugins, QML modules and shared libraries into `build/portable/Celestial-<version>-x86_64-portable.run`.

Packaging needs `qmake6`, Qt 6 runtime libraries/QML modules, `patchelf`, `ldd`, `readelf`, `tar`, `gzip`, `file`, `strings` and `sha256sum`. The local Qt libraries must be compatible with the remote-built executable.

`--container` uses the Ubuntu 22.04 environment in `packaging/portable/Containerfile` for bundling only. It cannot lower the remote binary's glibc/Qt requirements; an incompatible executable fails payload validation. The build prints the actual glibc floor.

The smoke test uses disposable configuration and runtime directories. It checks extraction, cache reuse, shared libraries and headless QML startup without accessing existing sync accounts.
