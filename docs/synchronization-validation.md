# Synchronization reliability: validation record

Date: 2026-10-07. Implements `docs/synchronization-plan.md` phases 1–7 on `develop`. Embedded rclone v1.64.2. All Rust builds, tests and Clippy ran on `rust-builder` through `cargo remote`; packaging ran locally with the remotely built executable.

This record lists what was verified, how, and what remains unverified. Tests use disposable local folders behind rclone `alias`, case-insensitive `local` and `memory` remotes; no cloud account or existing sync folder was used.

## Commands run

```sh
cargo remote -h -- test --locked                 # 156 passed, 1 ignored (the 20,000-file measurement), after the audit below
cargo remote -h -- clippy --locked --all-targets # clean with the manifest's deny settings
bash scripts/build-run.sh                         # after every phase commit
./scripts/build-native.sh --release
./scripts/build-portable.sh                       # Celestial-0.8.3-x86_64-portable.run, 73 MB
./scripts/smoke-portable.sh build/portable/Celestial-0.8.3-x86_64-portable.run   # passed
./scripts/build-portable.sh --container --output-dir "$PWD/build/portable-container"   # refused, see below
bash -n scripts/*.sh scripts/lib/*.sh
git diff --check
```

`cargo fmt` is not installed on this workstation; formatting used the builder's rustfmt 1.10 on a synchronized copy of the sources.

## Post-implementation audit

An audit of the implementation against the plan found the following defects. Each is fixed, and each listed test fails on the code before the fix and passes after it.

| Finding | Fix | Regression test |
| --- | --- | --- |
| Without a hash and without modification-time support (generic WebDAV), the precision is rclone's 100-year sentinel, so any two files of equal size compared as equal contents: divergent versions were recorded as synchronized without a conflict. | When no shared hash exists and modification times are not usable evidence, equal-size versions are downloaded into staging and compared. | `without_hashes_or_times_equal_sizes_are_compared_by_contents` |
| Keep both renamed one version but kept the item's history; if the copy that followed failed or was interrupted, the next pass saw a deletion that met an edit, restored it and recorded a spurious edit/delete outcome. A crash between the rename and its journal step also lost the conflict record. | The history of the renamed version is dropped with the rename; the replay recognises a completed rename by the copy, which carries the operation ID. | `an_interrupted_keep_both_is_not_mistaken_for_a_deletion` |
| An internal folder whose inode changed (the synced folder copied or restored) or that belonged to a stopped or nested pair was synchronized as ordinary contents, uploading every preserved version. | A recorded internal folder carrying this pair's marker is adopted; any `.celestial-*` folder carrying a marker is never synchronized. | `a_copied_internal_folder_is_adopted_not_uploaded`, `internal_folders_of_stopped_pairs_are_never_uploaded` |
| A remote Restore whose copy failed left the path empty after preserving the current version; the next pass propagated that as a deletion. A local Restore that displaced a concurrent edit kept it as an unrecorded copy. | The current version is moved back when the copy fails; the displaced edit gets a conflict record. | `a_failed_remote_restore_puts_the_current_version_back` |
| An upload interrupted after preserving the remote version but before journaling that step left the remote path empty, so the next pass restored it as an edit/delete outcome. Versions moved back stayed in the Restore list although nothing was stored there any more. | Preservation journals replay first; the upload replay finds the preserved version in the inventory and moves it back; moved-back versions leave the inventory. | `upload_interrupted_while_preserving_puts_the_remote_version_back` |
| A local file created while a new remote file was downloaded produced an extra conflict copy of the remote version, then a second one from the next pass's comparison. | The staged download is discarded and the next pass compares both versions once. | `a_local_file_created_during_a_download_is_compared_not_duplicated` |
| A file renamed only in letter case on a case-insensitive remote was blocked forever with advice to rename one of two names that did not coexist. | A case-only rename of a synchronized file is propagated when the old name is deleted and the new one created on the other side; the new name waits if removing the old one fails. Anything else (for example an edit to the old name) stays blocked. | `letter_case_renames_on_case_insensitive_remotes_are_propagated`, `a_letter_case_rename_that_met_an_edit_stays_blocked` |
| Uploads from a snapshot (remotes without a hash) carried the snapshot's creation time instead of the file's modification time. | The snapshot keeps the original modification time. | `uploads_to_remotes_without_hashes_use_a_snapshot` |
| While a remote was unreachable at startup, its capabilities were probed on every worker loop (twice a second). | Recorded capabilities are used when the probe fails; an unknown remote is probed again on its check schedule. | Reviewed; not covered by a test because it needs the worker loop. |

## Gap register outcomes

| ID | Outcome | Evidence |
| --- | --- | --- |
| G01 | Local changes use (size, mtime ns, ctime ns, inode, device); remote changes use the provider hash, else size and time at provider precision. Every transfer sets `IgnoreTimes`. Signed seconds plus nanoseconds are stored; pre-epoch and post-2038 times work. Racy-clean rule uses the inode change time (2 s window). | `same_size_edit_with_preserved_mtime_is_detected`, `same_size_remote_edit_with_preserved_mtime_is_detected`, `timestamps_after_2038_and_before_1970_are_recorded`, `racy_files_are_rechecked_before_their_tuple_is_trusted`, `stamps_before_the_epoch_and_after_2038_order_correctly` |
| G02 | Identical contents advance the baseline without conflict or transfer. | `identical_concurrent_edits_converge_without_conflict`, `metadata_only_changes_do_not_transfer` |
| G03 | First sync and legacy history never pick a winner by time: equal contents are recorded, different contents keep both. Legacy rows migrate as `needs_verification` and never authorize deletion. | `first_sync_of_equal_contents_records_without_copying`, `first_sync_with_different_contents_never_overwrites_by_timestamp`, `legacy_history_with_a_missing_side_is_not_a_deletion`, `existing_history_needs_verification_and_pairs_get_identities` |
| G04 | Automatic Keep both for edit/edit, bootstrap and file/folder conflicts, with persistent conflict records; Keep local/remote removed. | `different_versions_restored_on_both_sides_keep_both`, `equal_modification_times_move_the_remote_version`, `file_and_folder_with_one_name_are_both_kept`, `acknowledging_a_conflict_removes_it` |
| G05 | Recovery inventory for every replacement and deletion on both sides, restore, fixed retention (10 versions, 30 days, protected cases). | `propagated_deletions_are_recoverable_on_both_sides`, `replaced_versions_are_preserved_in_both_directions`, `retention_keeps_recent_versions_and_the_only_copy_of_deleted_items`, `versions_of_unresolved_conflicts_are_never_pruned` |
| G06 | Edit/delete deletion is bound to a content token; changed tokens are refused. | `deletion_after_an_edit_delete_outcome_is_bound_to_what_was_shown` |
| G07 | Both sides re-observed before replacing or deleting; journaled operations; replay at each pass; only transferred bytes acknowledged; staging inside an ownership-marked internal folder; stray partial files discarded. | `source_changed_before_the_transfer_is_retried_not_copied`, `interrupted_*`, `download_interrupted_*`, `upload_interrupted_after_preserving_puts_the_remote_version_back`, `stray_partial_files_in_staging_are_removed_not_uploaded`, `leftover_partial_downloads_are_not_uploaded` |
| G08 | Type replacements preserve the displaced file and remove folders only when empty. | `unchanged_file_replaced_by_folder_is_propagated`, `unchanged_folder_replaced_by_file_is_propagated`, `remote_folder_deletion_preserves_changed_local_children`, `local_folder_deletion_preserves_changed_remote_children` |
| G09 | Edit/delete restores the edit and records the outcome; delete/recreate after a retired tombstone is a new item. | `remote_update_after_local_delete_restores_without_duplicates`, `history_for_items_missing_on_both_sides_is_retired_after_complete_pass` |
| G10 | Capabilities recorded per remote from `operations/fsinfo`; fallbacks exercised for no hash, coarse precision, case-insensitive names and no server-side move. Residual remote window documented in README. | `capabilities_are_recorded_per_remote`, `uploads_to_remotes_without_hashes_use_a_snapshot`, `without_hashes_equal_size_and_time_count_as_equal_contents`, `case_insensitive_backends_are_detected_and_collisions_blocked`, `backends_without_server_side_move_still_preserve_versions` |
| G11 | Case, normalization, duplicate-name and non-UTF-8 items are blocked individually with a reason; conflict names fit 255 bytes. | `case_collisions_on_case_insensitive_remotes_are_blocked`, `unicode_normalization_variants_are_blocked`, `non_utf8_names_are_blocked_not_mapped_to_empty_paths`, `duplicate_and_escaping_remote_paths_are_blocked`, `long_names_get_conflict_copies_that_fit` |
| G12 | Debounce (250 ms quiet, 2 s maximum), events during scans keep pairs dirty, internal-folder writes ignored, nested roots, offline recovery. | `bursts_are_debounced_with_a_maximum_delay`, `an_event_during_a_scan_leaves_the_pair_dirty`, `nested_roots_are_both_marked`, `atomic_save_storms_converge_to_the_last_version`, `an_offline_remote_pauses_the_pair_and_recovers_without_action` |
| G13 | One listing and one history read per pass; identical snapshots and tray states not republished; exclusions read on change; progress throttled; per-pass statistics. | `an_idle_pass_reads_no_contents_and_lists_once`, `identical_snapshots_and_tray_states_are_not_republished`, `exclusions_are_reread_only_when_the_file_changes`, measurement below |
| G14 | Local, crash and packaging gates verified as listed here; live providers and interactive desktop use not verified. | See "Not verified" |
| G15 | Database enumeration/read failures are visible errors; RPC replies decoded fallibly; invalid remote names are errors; history deletion errors reported. | `settings_read_failures_are_errors_not_empty_configuration`, `malformed_remote_names_are_errors`, `remote_names_with_separators_are_errors_not_panics`, `database_read_failure_cannot_turn_tracked_edit_into_initial_sync`, `database_write_failure_is_reported` |
| G16 | No pair-wide halt: items retry with backoff from 1 minute to 1 hour, after dismissal and after restart; pair-level problems are rechecked automatically. | `one_failing_item_does_not_stop_the_rest_of_the_pair`, `transient_failures_back_off_and_retry_automatically`, `retries_back_off_from_one_minute_to_one_hour` |

## Operation faults and concurrency

Failpoints (`src/sync/failpoint.rs`) interrupt an operation by panicking, which skips ordinary error cleanup like a crash, or inject an edit inside a commit window:

- `preserve_local:after_move`, `preserve_remote:after_move`: the moved version is recorded on the next pass.
- `download:staged`: the local file is untouched; the stage is discarded.
- `download:exchanged`: the new version is in place; the displaced version is preserved.
- `download:before_exchange` with a local edit: the edit becomes a conflict copy.
- `upload:before_transfer`: the preserved remote version is moved back.
- `preserve_remote:before_move` with a remote edit: the edit becomes a conflict copy.
- `keep_both:renamed`: the conflict record is written and both versions converge.

Path-to-link swaps are refused by descriptor-based resolution (`links_inside_the_pair_are_not_followed`, `a_folder_swapped_for_a_link_after_observation_is_not_followed`); a replaced root is refused (`a_replaced_root_is_refused`). A listing that misses an existing item does not delete it (`an_item_missing_from_a_delayed_listing_is_not_deleted`).

## Measurements

`measure_twenty_thousand_files` on rust-builder (4 cores, 8 GiB), 100 folders × 200 files, local alias remote:

| Pass | Wall time | rclone RPCs | Local bytes hashed |
| --- | --- | --- | --- |
| Initial upload of 20,100 items | 32.2 s | 60,302 (three per file: re-check, copy, verify) | 0 |
| Idle pass, full local walk | 4.1 s | 1 (one recursive listing) | 0 |

The idle time includes the alias backend computing MD5 for every listed file, which cloud providers report from metadata instead. The audit's historical timings are superseded by these figures. A watched idle pair additionally skips the local walk and reuses its last observation until an event arrives or the 10-minute full scan is due.

## Release and portable bundle

- Fresh release executable after every phase commit; `target/debug` removed each time.
- Portable bundle built and smoke-tested: extraction, cache reuse, library closure and headless QML start passed.
- Actual ABI floor: GLIBC_2.39, GLIBCXX_3.4.30, CXXABI_1.3.15 (determined by rust-builder).
- Ubuntu 22.04 container bundling refuses the executable with explicit missing-version errors (GLIBC_2.38/2.39, CXXABI_1.3.15). Ubuntu 22.04 runtime support is not claimed.

## Residual limits

- Remote replacement moves the current version into recovery and then uploads, rather than passing `BackupDir` to `copyfile`; an edit another client makes between that move and the end of the upload is replaced (the documented residual remote window).
- rclone reads and writes local files by path. Celestial checks the item's identity through the open pair root before and after each transfer and hash, so a folder swapped for a link is detected and the result is not recorded, but the transfer itself is not confined to the descriptor.

## Not verified

- Live providers (Drive, Dropbox, pCloud, Proton Drive, Nextcloud/ownCloud, generic WebDAV): no disposable test credentials were available. Interrupted transfers, recoverable deletion and overwrite, type transitions and concurrent edits from two clients remain unverified against real backends. Backend behavior is derived from the embedded rclone sources and the capability fixtures above.
- Interactive desktop use: the new conflict, Delete everywhere and Restore controls compile and the QML loads headless, but they were not exercised by hand on a desktop session with the tray.
- Disk-full and truncated transfers: free space is checked before writing and transfers are verified afterwards, but no full filesystem or truncated network transfer was injected.
- A backend that reports duplicate names was not available as a fixture; duplicate handling is covered at the observation layer.
