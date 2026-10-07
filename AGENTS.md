## Remote Rust builds

Compile with `cargo remote` instead of a local `cargo build`, `cargo test`, or `cargo clippy`. One command syncs the sources, builds on the shared builder, and copies back only the finished executable into `target/<profile>/`. Crates, incremental files, and the rest of `target/` stay on the builder.

```bash
cargo remote -- build --release
cargo remote -- test
cargo remote -- clippy --all-targets
```

`Cargo.toml` already denies warnings. cargo-remote strips a second `--`, so forwarding `-- -D warnings` fails with `unexpected argument '-D'`.

`cargo fmt` stays local. Dotfiles are not synced; pass `-h` only when the build needs them. Pass `-c <path-under-target>` only when you explicitly want a different file copied back.

The builder is already set up. Do not compile locally because a host is missing from Cargo's config. `cargo remote` does not read `~/.cargo/config.toml` for the host. That file only sets the `kache` wrapper, and a host will not be there.

Read these two files directly. A search that stays inside this repo will not see them, and that is not evidence that the builder is unconfigured.

- `~/.config/cargo-remote/cargo-remote.toml` — the only config `cargo remote` uses. The filename is `cargo-remote.toml`, not `config.toml`. It defines one remote, `rust-builder`, so `cargo remote -- build` uses it with no `-H` and no `-r`. Builds land in `~/remote-builds` on that machine.
- `~/.ssh/config`, host `rust-builder` — `192.168.0.65`, user `builder`, key `~/.ssh/id_ed25519`.

If `cargo remote` fails, report the command's error. Do not switch to a local `cargo build`, `cargo test`, or `cargo clippy`.

## After every commit

After every commit you MUST ALWAYS build a fresh release binary and delete the debug binaries. Debug builds must not take up local disk space.

```bash
bash scripts/build-run.sh
rm -rf target/debug
```

`scripts/build-run.sh` builds the release remotely and leaves only `target/release/celestial` and `target/release/celestial.run`. `cargo remote -- test`, `build` and `clippy` copy debug binaries back into `target/debug`, so remove that folder even if you did not build it explicitly. If the release build fails, report the error instead of committing more work on top of it.
