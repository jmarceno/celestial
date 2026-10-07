# Synchronization audit — 2026-10-07

This review uses source inspection and disposable local rclone remotes with an in-memory SQLite database. Existing cloud accounts, configuration and user files are not accessed. No changes are installed into the running application.

## Confirmed defects repaired

- The single-file removal helper called `operations/delete`, which operates on the remote filesystem. It now calls `operations/deletefile` with the individual path. [rclone RPC reference](https://rclone.org/rc/#operations-deletefile-remove-the-single-file-pointed-to).
- Normal file uploads/downloads incorrectly treated two files as different types and removed the destination before copying. Celeste no longer deletes the destination before ordinary copies; the missing-source failure regressions preserve the previous copy. Backend behavior during mid-transfer failures still needs integration validation.
- Unchanged parent-folder timestamps prevented traversal into changed children. Both local and remote child trees are checked independently of the parent's timestamp.
- Some history, conflict and stop-folder queries omitted the selected sync-pair/account ID. Identical paths on different accounts now keep independent histories.
- Stop/remove commands discarded database errors and updated the UI before database success. Record removal is now transactional and failures remain visible.
- Remote deletion removed history on failure; successful remote updates could fall through to a duplicate INSERT. The success/error branches now retain history appropriately and update existing records once.
- An unavailable local root could allow the remote pass to interpret missing local files as deletions. Loss of either selected root now pauses the pair.
- Recursive folder deletion trusted the parent timestamp. It now checks descendant baselines and pauses if the remaining tree contains changed, untracked or excluded items.
- Exclusion rules used inconsistent path prefixes, unreadable rules were treated as empty, and the rules file itself could synchronize. Both sides now use paths relative to the local pair root; invalid/unreadable rules pause the pair and `.sync-exclude.lst` remains local.
- A new download could save the pre-transfer local timestamp. The baseline now records the actual post-transfer metadata, and baseline-write errors are reported.
- Remote path traversal and symlink targets could escape the local pair. Traversal paths are rejected and automatic passes do not follow symlinks inside the pair.
- Startup errors returned by the worker were silently discarded. They now appear in the error dialog.

## Monitoring and efficiency

The application has **no filesystem watcher**. After each pass it waits up to **500 ms**; a UI command can wake it earlier. This is a minimum delay between passes, not an exact two-checks-per-second guarantee.

The original change probe walked/opened local files, queried SQLite per file, recursively listed the remote, then issued additional remote stat calls per tracked item. It also compared remote basenames against stored full paths and could prepend the sync-folder prefix twice, causing clean non-root pairs to appear dirty.

The current probe reads local metadata without opening file contents, loads the pair's history once and makes one recursive remote listing. Hash maps compare the snapshots and a hash set avoids quadratic searches for files already processed in the pass. Root availability adds one remote stat per pair. History is retired only after both complete snapshots confirm an item is absent; failed observations preserve it.

Local traversal and recursive remote listing still happen during idle checks. Active passes still perform individual metadata/transfer RPCs and publish frequent per-file status snapshots/tray updates. Coalescing identical UI snapshots is another possible optimization. This review does not claim measured improvements on a production cloud account.

A conservative next implementation should:

1. Install a recursive local watcher before the initial scan and debounce events into dirty sync-pair IDs. Preserve the current reconciliation decisions initially.
2. Treat events as hints, with periodic full reconciliation and fallback polling for missing events, watcher overflow, unsupported/network filesystems and unavailable roots. Watch the parent directory to detect removal of the selected root. [notify's documented caveats](https://docs.rs/notify/latest/notify/#known-problems).
3. Keep scheduled remote checks: a local watcher cannot observe edits made by other devices. Provider change notifications are capability-dependent; rclone exposes `ChangeNotify` through [filesystem information](https://rclone.org/rc/#operations-fsinfo-return-information-about-the-remote).
4. Verify atomic-save/rename bursts, simultaneous local/remote edits, exclusions, offline recovery, watcher overflow and large idle trees in isolated fixtures before replacing the current polling schedule.

No watcher dependency, polling interval change, sync-engine replacement or database schema migration is introduced in this pass.

## Remaining limitations

- File change detection uses whole-second modification times. Same-second edits or contents changed while preserving modification times can be missed; hashing/size and higher-resolution baselines need a separately validated schema/design change.
- A conflict choice is revalidated against the versions shown in the prompt, but the copy itself can still race with an edit made after that final check.
- Snapshot reads, descendant validation and subsequent copies/deletions are separate operations. Concurrent edits can occur between them. File/directory type replacements still need staged replacement to preserve the old destination on every failure. Provider versioning/conditional writes and staged local transfers need broader integration tests.

## Deprecated components removed

Removed Snap/makedeb definitions and publishing workflows, the old package-based CI setup, unused GTK tray/icon artwork, screenshot and legacy AppStream metadata, unused translation catalogs/build recipes, an unused OAuth template, template-app renaming files, and the obsolete panic-wrapper CLI entry point. The portable bundle also excludes Qt's optional GTK theme plugin so it does not pull GTK libraries into the runtime closure. Build/staging paths and docs now reflect Qt Quick and the configured remote builder. The public CI workflow checks formatting and shell syntax; Rust tests and Clippy run on the private builder.

The stable app ID, config directory, current logo, active OAuth assets, original license/copyright, release history and database migrations are retained because current functionality or existing data depends on them. Qt may still depend on GLib; GLib runtime libraries are not obsolete GTK application code.

## Follow-up validation

Each claim above was checked against the source and, where it describes behavior, against a regression test on the remote builder. All "confirmed defects repaired" items are present in the code and covered by tests, with two exceptions:

- **Exclusions (partially repaired).** The remote pass matched patterns directly instead of using the shared exclusion check, so a remote `.sync-exclude.lst` was synchronized and could replace the local rules. Fixed; the remote pass now uses the same check as the probe and the local pass.
- **Root loss (partially repaired).** A missing root paused the pair, but an empty root (an unmounted drive's mount point, or a provider returning an empty listing) still looked like the deletion of every item and was propagated. The change probe now pauses the pair when one side has no items while the other still holds tracked ones.

These limitations from the list above are now resolved:

- Timestamps are stored as 64-bit values. SQLite `INTEGER` columns already hold 64 bits, so no column migration was needed. Files with mtimes after 2038 previously got a baseline of `0` and were rechecked on every probe.
- Any difference from the recorded baseline counts as a change, in either direction. Restored older versions now propagate, and restores on both sides become a conflict.
- Conflict prompts record the versions they show. A choice is refused if either item has changed since then, if the local item is reached through a symbolic link or no longer maps into the pair, or if a file and a folder share the name.
- A migration keeps the oldest row of each duplicate history entry (the one synchronization kept updating) and adds a unique index on `(sync_dir_id, local_path, remote_path)`.

Startup failures now appear under their own title instead of "Unknown Error" with a request for a backtrace. The Clippy command in `AGENTS.md` now omits the forwarded `-- -D warnings`, which cargo-remote cannot pass through.

One suspected issue did not reproduce: folder baselines do converge after a child is added while the parent's timestamps change on both sides.

## Validation

- 44 remote tests pass: the original 7, 23 isolated synchronization regressions, one path-traversal test and 13 tests added by the follow-up validation (empty roots, remote exclusion file, restored versions, post-2038 timestamps, conflict revalidation and the duplicate-history migration).
- Local formatting, shell syntax and whitespace checks pass.
- Remote Clippy (all targets, manifest warning-denial settings) and the release build pass. The originally requested forwarded `-- -D warnings` command fails because cargo-remote strips its separator; the successful retry omits those forwarded arguments.
- The regenerated portable bundle passes extraction, cache reuse, shared-library closure and headless QML startup with disposable configuration/runtime directories. No GTK libraries or GTK theme plugin remain in its payload. The bundle requires glibc 2.39 or newer; the Ubuntu 22.04 packaging-container path was not exercised.
- Tests exercise the linked librclone 1.64.2 with temporary local alias remotes. Live cloud-provider synchronization is not exercised.
