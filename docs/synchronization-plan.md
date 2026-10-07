# Synchronization reliability implementation plan

Date: 2026-10-07. Reviewed checkout: `2a970e1` (source unchanged at `d9d3a56`). Embedded rclone: v1.64.2, read from the vendored `librclone-sys 0.6.3+rclone-v1.64.2` sources.

Status: planning only; none of the changes below are implemented by this document. This plan replaces `docs/synchronization-audit.md`, whose original evidence remains in Git history. Current status was checked against source, existing test definitions and the vendored rclone sources; builds, tests, cloud accounts and running application data were not accessed during this review. Earlier test counts and timings are historical evidence, not fresh validation. Keep plans and audits under `docs`; do not link them from README or AGENTS.md.

## Goal and scope

Detect content changes reliably, preserve recoverable versions before replacement or deletion, keep unaffected items synchronizing when one item fails, and prevent stale observations from authorizing destructive operations. Cover every outstanding item from the audit and the Syncthing comparison while retaining the existing Rust worker, Qt Quick UI, rclone integration, account isolation, exclusions and tray behavior.

What Syncthing parity actually requires: Syncthing detects concurrent changes with per-device version vectors, rescans metadata (size, nanosecond mtime and, since 1.21, inode change time) and hashes only files whose metadata changed. A conflict never stops synchronization: both versions are kept automatically, the one with the older modification time is renamed to `<name>.sync-conflict-<date>-<time>-<device>.<ext>`, and the conflict copy synchronizes like any other file. Failed items are retried automatically while the rest of the folder continues. Syncthing has no compare-and-swap against other devices and does not periodically rehash unchanged files. [Syncthing protocol](https://docs.syncthing.net/specs/bep-v1.html#fields-fileinfo-message), [conflict handling](https://docs.syncthing.net/users/syncing.html#conflicting-changes).

Celestial synchronizes a local folder with arbitrary cloud storage, including edits from clients that publish no version vectors. Use a last-common-content baseline, per-pair generations and provider content hashes to distinguish independent changes. These generations order Celestial's own operations; they are not distributed version vectors. Wall-clock time may choose which version keeps the original name in an automatic Keep both, as in Syncthing, but never which content survives.

## Verified platform constraints (rclone v1.64.2 via librclone RPC)

These facts bound the design; do not plan around APIs the embedded runtime lacks.

- Available: `operations/list` and `operations/stat` with `opt` (`showHash`, `hashTypes`, `showOrigIDs`, `metadata`, `filesOnly`); `operations/fsinfo` (features, hash set, precision); `copyfile`, `movefile`, `deletefile`, `purge`, `mkdir`, `rmdir`, `rmdirs`, `cleanup`; `sync/*`; `core/version`. Every call accepts `_config` overrides such as `BackupDir`, `Suffix`, `IgnoreTimes`, `CheckSum` and `UseListR`.
- Not available: `operations/hashsum`, any change-notification subscription (ChangeNotify is reachable only inside VFS/mount), conditional (If-Match) create/replace/delete, ETags or revision IDs. `ID` is reported only by some backends and identifies an object, not a version; a Drive file ID survives content edits.
- `copyfile` skips the transfer and reports success when the destination's size and modification time match within the backend's precision (`NeedTransfer` → "Size and modification time the same"). Today a detected same-size, same-second change, including a conflict resolution, can therefore leave the destination unchanged while Celestial records it as synced. Every intended transfer must pass `_config: {"IgnoreTimes": true}`.
- Downloads to the local filesystem are written to `<name>.<8 random chars>.partial` and renamed over the destination (local backend: `PartialUploads` + `Move`, `Inplace` off). The final rename replaces whatever is at the destination at that moment. rclone deletes the partial file on ordinary failure, but a crash or kill leaves it in the pair, and the next pass uploads it as a user file.
- Uploads from the local filesystem fail with "source file is being updated" when the file's size or mtime changes while it is read. After a transfer, rclone compares size and, when both sides share a hash type, a hash computed by rereading the local file; a mismatch fails as "corrupted on transfer" unless `IgnoreChecksum` is set.
- `copyfile` with `_config.BackupDir` (same remote, not the destination directory) or `Suffix` moves the existing destination object aside before overwriting it, whatever its current contents. `deletefile` ignores backup settings; a recoverable remote deletion is a `movefile` into a recovery path.
- Hashes and precision for the login kinds Celestial offers: Google Drive MD5, 1 ms; Dropbox DropboxHash, 1 s; OneDrive QuickXorHash, 1 s; pCloud MD5+SHA-1 (EU: SHA-1+SHA-256), 1 s; Proton Drive SHA-1, 1 s; WebDAV MD5/SHA-1 only for Nextcloud/ownCloud servers, otherwise none, and generic WebDAV cannot set modification times. Local hashes in any supported type can be computed through `operations/stat` with `showHash`/`hashTypes` on fs `/`, so Celestial need not reimplement provider hash algorithms.
- Provider trash varies (Drive `deletefile` trashes by default). Record it as an observed capability; never treat it as Celestial's recovery guarantee.

## Existing audit: disposition

“Present” below means the repair exists in the inspected code and relevant tests exist where listed. It does not mean those tests were rerun or that every cloud backend has been validated. Preserve these fixes as regressions throughout implementation.

| Audit finding | Current evidence and status | Work in this plan |
| --- | --- | --- |
| Single-file deletion used a filesystem-wide operation | Present: `src/rclone.rs` uses `operations/deletefile`; `deleting_one_remote_file_preserves_neighbors_and_other_folders` | Retain; recoverable remote deletion in phase 4. |
| Normal file copies deleted the destination first | Ordinary same-type copies no longer remove it; `failed_upload_preserves_existing_remote_copy` and `failed_download_preserves_existing_local_copy` exist. Downloads are atomic through rclone's `.partial` rename; upload atomicity depends on the backend's `PartialUploads` feature. Orphaned `.partial` files and the clobbering rename remain. | Staging, recovery and provider failure tests in phases 3–4 and 7. |
| Unchanged parent timestamps hid changed children | Both walks recurse independently; local/remote child-edit tests exist. | Retain; content checks in phase 2. |
| History/conflict/stop queries crossed account or pair boundaries | Scoped queries and cross-remote tests are present. | Extend the same isolation to fingerprints, journals, conflicts and recovery stores in phase 1. |
| Stop/remove changed the UI before database success | `process_deletions` uses transactions; failure tests exist. Stopping a pair deletes its whole history, so re-adding it is a first sync. | Retain; safe bootstrap in phase 2; pending operation recovery in phase 4. |
| Failed deletion lost history; updates inserted duplicates | Success-dependent removal and baseline updates are present; failure/duplicate tests exist. `delete_db_entry` still discards database errors. | Retain; journal commit protocol and visible errors in phase 4. |
| Missing or empty roots caused mass deletion | Availability checks and empty-root guards are present; both directions have tests. | Retain; root identity and destructive-operation checks in phases 4 and 7. |
| Recursive deletion trusted only the parent | Descendant guards and changed-child tests are present. Validation and `remove_dir_all`/`purge` still race. | Per-item guarded deletion in phase 4. |
| Exclusions used inconsistent paths or synchronized the rules file | Shared root-relative matching, rule-read failures and local-only `.sync-exclude.lst` protection are present. | Retain; internal recovery paths and exclusions tests in phases 3 and 7. |
| Download baselines used pre-transfer metadata; write errors disappeared | Actual destination timestamps and visible baseline-write errors are present. Upload baselines reread local metadata after the transfer, so an edit finishing between the upload and that read is acknowledged without being transferred. | Verified content baselines in phases 2 and 4. |
| Traversal paths or symlinks escaped the pair | `local_path_for_remote`, `validate_local_path` and regression tests are present. Path validation is separate from I/O. | Descriptor-based local I/O and path-swap tests in phase 4. |
| Startup errors were discarded or mislabeled | Worker startup errors have a specific fatal dialog in `spawn`. | Preserve; test alongside visible runtime observation failures in phase 7. |
| Whole-second timestamps miss edits | Still pending: probes, transfers and `ItemVersion` use whole-second timestamps although rclone returns full provider precision; `RcloneRemoteItem` omits size, hashes and ID. `copyfile` additionally skips same-size/same-mtime destinations. | Phases 1–2 and 4. |
| Conflict choice can become stale during copying | Timestamp/type revalidation is present; the check-to-copy race remains. | The Keep local/Keep remote choice is removed in phase 3; remaining user actions use token binding and guarded commits in phases 3–4. |
| File/folder replacement can remove the old item before a failing transfer | Still pending in `push_local`, `pull_local`, `push_remote` and `pull_remote`. | Phase 4. |
| Watchers, fallback reconciliation and scheduled remote checks | Implemented in `src/sync/watch.rs` and `RemoteChecks`: recursive watch installed before scanning, root identity check before each pair decision, unsupported-filesystem/error fallback, 10-minute full metadata scans, 60-second idle remote listings. An event during a scan leaves the pair dirty, so it cannot be certified clean. | Retain; debounce and validation in phase 5. |
| Frequent per-file RPCs, snapshots and tray updates | Still present: one `stat` RPC, one history query and one `exists` query per local item; every progress update rebuilds the JSON snapshot, rereads every pair's exclusion file and calls the tray. | Phase 6. |
| Live cloud behavior, production efficiency and packaging-container path | Earlier validation used temporary local aliases. Cloud behavior and Ubuntu 22.04 container bundling were not exercised. | Explicit gates in phase 7. |
| Post-2038 timestamps, restored older versions and duplicate history | 64-bit fields, comparisons in both timestamp directions and `m20261007_000001_sync_items_unique_paths` are present with tests. Pre-epoch local mtimes fail `duration_since(UNIX_EPOCH)` and are reported as errors. | Keep existing migrations unchanged; signed timestamps in phase 1; regressions in phase 7. |
| Deprecated components and branding/config migration | Prior removal and migration are already represented in the checkout; no synchronization work remains on that cleanup. | Preserve active OAuth/assets, legacy config migration and portable delivery in phase 7. |
| Suspected failure to converge folder baselines | Not reproduced by the original audit; do not label it a confirmed defect. | Retain a convergence regression in phase 7. |

The old audit's proposed watcher implementation and statements that idle pairs always traverse both trees are superseded by the current watcher and remote schedule. Remaining watcher work is hardening and validation, not reinstalling the existing mechanism.

## Complete gap register

| ID | Gap or outstanding validation | Planned closure |
| --- | --- | --- |
| G01 | Same-second, preserved-mtime, backwards-clock, pre-epoch and coarse-provider timestamp handling; `copyfile` skipping same-size/same-mtime destinations | Precise metadata, inode change time, provider hashes and forced transfers: phases 1–2 and 4. |
| G02 | Both timestamps change but bytes are identical | Compare contents and advance history without a conflict: phase 2. |
| G03 | First sync or lost/legacy history selects by timestamp, including remote-on-tie | Safe bootstrap with Keep both for differing contents: phases 1–2. |
| G04 | Keep local/remote destroys the other version; no Keep both, automatic resolution or durable conflict record | Automatic Keep both for every edit/edit and bootstrap conflict, with persistent conflict records: phase 3. |
| G05 | No recoverable history for ordinary replacements and propagated deletions | Recovery inventory, restore and explicit retention: phases 3–4. |
| G06 | Prompt revalidation cannot detect content changes that preserve metadata | Bind every user action to content tokens and reject stale tokens: phases 2–4. |
| G07 | Source, destination, descendants, paths and roots can change after observation; baselines can acknowledge bytes never transferred; orphaned rclone `.partial` files are uploaded | Guarded commits, journal recovery, verified baselines and owned staging: phase 4. |
| G08 | File/folder replacement removes the old destination before success | Stage and quarantine both replacement directions: phase 4. |
| G09 | Edit/delete and delete/recreate lack an explicit persistent reconciliation policy | Presence states, tombstones and content-aware decisions: phases 1–4. |
| G10 | Hashes, precision, partial uploads, server-side move and trash vary by backend; the embedded rclone offers no conditional writes or change notification | Capability records, residual-race policy and safe behavior per capability: phases 1, 4–5 and 7. |
| G11 | Case/normalization collisions, duplicate remote names and non-UTF-8 local names (today mapped to an empty path by `to_str().unwrap_or("")`, or failing the whole probe) | Collision and encoding checks, safe naming: phases 1–4. |
| G12 | Atomic-save bursts, watcher overflow/root lifecycle and offline recovery have incomplete coverage | Debounce, reconciliation and fault tests: phases 5 and 7. |
| G13 | Avoidable remote work, JSON generation, exclusion reads and tray updates | Instrumentation, safe caching and coalescing: phase 6. |
| G14 | Local alias tests/headless startup do not establish live cloud, failure or desktop behavior | Provider, crash, UI, performance and portable gates: phase 7. |
| G15 | Runtime database enumeration failures can look like empty configuration; malformed RPC JSON and trailing-colon remote names panic; history deletion errors are discarded | Explicit observation errors and fallible decoding: phases 1, 4 and 7. |
| G16 | Any error in a pair, including a transient network failure or one conflict, stops the whole pair (`has_errors` skips it) until the user dismisses every error; errors and conflicts live only in memory | Per-item blocking, automatic retry with backoff and persistent item state: phases 2, 3 and 5. |

G15 was confirmed during this review: `worker`/`reload_model` use `unwrap_or_default`, `SyncDirsModel::exists` collapses errors into absence, RPC response decoding in `src/rclone.rs` uses `unwrap`, `get_remote_name` panics, and `delete_db_entry` ignores failures. Address these as part of uncertain-state handling rather than treating failures as successful empty observations.

G16 was confirmed in `worker`: a pair with any recorded error is skipped and its remote check forgotten until the error count returns to zero. This is the largest behavioral difference from Syncthing.

## Phase 1 — Versioned observations and persistent state

Targets: `src/entities/sync_items.rs`, new additive migrations/entities, `src/migrations/mod.rs`, `src/rclone.rs`, `src/sync/model.rs`; introduce focused observation/reconciliation modules under `src/sync`.

- Define observations with explicit `Present`, `Missing` and `Unknown/Error` states. A failed/incomplete read is never evidence of deletion. Store file/folder kind, size, modification time as signed seconds plus nanoseconds, provider precision, and local device/inode/ctime. Keep timestamps after 2038 and before the Unix epoch representable.
- Extend `RcloneRemoteItem` with `Size`, `Hashes` and `ID`, and request hashes in the listings that already run (`showHash` with the backend's preferred type), so remote content identity costs no extra calls. The remote change token is (kind, size, hash when present, modification time at provider precision); `ID` identifies the object for rename and duplicate-name handling only.
- Store content fingerprints with algorithm and provenance. Only compare hashes with the same algorithm; never interpret a missing hash as empty-file contents.
- Persist the last common verified contents, last observations, pair generation, deletion tombstones, per-item state (synced, pending, retrying with next attempt time, conflict, blocked), conflicts and operation/recovery records. Keep `(sync_dir_id, local_path, remote_path)` uniqueness and a stable pair identity for recovery storage. Tombstones record confirmed deletion, not listing failure; retain them through offline/retry periods and until reconciliation is acknowledged.
- SeaORM already versions migrations in `seaql_migrations`; keep old migrations intact and add new ones additively. Copy the database file aside before applying a schema change and report a failed copy instead of migrating. Existing timestamp-only rows become `NeedsVerification`, never invented content baselines. If current sides differ and the former common contents cannot be established, apply the bootstrap rule from phase 2; a missing side during this bootstrap must not authorize deletion of the other. Both sides absent is retired only after complete successful observations, as `directory_changed` does today. Version journal payloads and reject unknown formats; document downgrade as requiring the matching database backup and preservation of recovery assets.
- Record backend capabilities from `operations/fsinfo`: hash set, precision, `CaseInsensitive`, `DuplicateFiles`, `PartialUploads`, `Move`/`Copy` (server-side), `CanHaveEmptyDirectories`, and whether `SetModTime` works. Record observed trash behavior separately. Do not add fields for conditional writes or change notification; the embedded runtime exposes neither.
- Read `core/version` at startup and log it. Any rclone upgrade is an explicit, locked dependency change that reruns the capability tests in phase 7; newer rclone documentation is not evidence for the embedded runtime.

Acceptance: old databases migrate without erasing history; unknown observations cannot trigger transfer/deletion; two accounts with identical paths remain independent; capabilities are recorded per remote.

## Phase 2 — One content-aware reconciliation algorithm

Targets: the local/remote probes and walks in `src/sync/engine.rs`, observation/reconciliation modules, `src/rclone.rs`, `src/sync/watch.rs`.

Replace the duplicated timestamp decision trees in `sync_local_directory` and `sync_remote_directory` with one decision function over both observations and the verified common baseline. Metadata and watcher events schedule work; content identity decides whether contents changed. Freshly observe both sides immediately before an operation that could replace or delete an existing item.

| Observed state | Required decision |
| --- | --- |
| No trusted common baseline; both regular files with equal contents | Establish a verified common baseline without copying. |
| No trusted common baseline; both exist with different sizes, contents or kinds | Keep both (phase 3) without overwriting either. Timestamp ties and clock skew do not select a winner. |
| No history at all; only one side exists | Create the missing copy with no-replace creation after successful root/absence checks. Legacy or damaged history uses the bootstrap rule. |
| Both equal the common contents | No content transfer. Refresh metadata as needed. |
| Exactly one side differs from the common contents | Propagate that version through the guarded transfer protocol. |
| Both changed to identical contents | Advance the common baseline without a conflict or redundant upload. |
| Both changed to different contents | Edit/edit conflict: automatic Keep both (phase 3). |
| One deleted; surviving contents equal the verified baseline | Propagate deletion through recoverable, guarded removal. |
| One deleted; surviving contents changed | Restore the edit to the deleted side and record the edit/delete outcome; the user may then delete it explicitly, which archives it first. |
| Both confirmed absent | Acknowledge the tombstone and retire active history only after complete observations; retain recovery data according to policy. |
| Any observation uncertain or the item still changing | Mark only that item retrying with backoff; continue with the rest of the pair; do not advance its history. |

- Local change detection uses (size, mtime ns, ctime ns, inode, device). Tools that preserve mtime (`cp -p`, `rsync -t`, `touch -d`) still change ctime, which user space cannot set. Hash a local file only when that tuple changed, during bootstrap and before a destructive operation that depends on its contents.
- Apply a racy-clean rule as Git does: a file whose mtime or ctime is not older than the observation by at least the filesystem's timestamp granularity (use 2 s) is not trusted as unchanged; recheck it on the next pass before using its fingerprint as a baseline.
- Remote change detection uses the provider hash from the listing when the backend has one. Without a hash (generic WebDAV), use size and modification time and document that a same-size edit within the provider's precision is undetectable without downloading. Different sizes prove different contents without any hashing; compare sizes first.
- Compute local hashes in the provider's algorithm through `operations/stat` with `showHash`/`hashTypes` on fs `/`. For mutable files, compare the local tuple before and after hashing; a moving target is retried, not fingerprinted.
- Do not add periodic full-content rehashing. It is not needed for Syncthing parity once ctime and provider hashes are used, and on no-hash remotes it would mean downloading every file. An opt-in, bandwidth-limited scrub can be offered later and must be documented as such.
- Reject ambiguous mappings before mutations: case-insensitive collisions on `CaseInsensitive` remotes, Unicode normalization collisions, multiple remote objects at one path on `DuplicateFiles` remotes, and non-UTF-8 local names. Block only the affected item with a visible reason and rename guidance; never map a name to an empty or lossy path. Directory comparisons use descendants, not directory mtime.
- Remove the pair-wide halt: `has_errors` must not skip a pair. Items in conflict, blocked or retrying are skipped individually; transient failures retry automatically with capped exponential backoff (proposed 1 min to 1 h); root-level failures (unavailable root, emptied root, unreadable exclusions, database failure) still pause the whole pair.

Acceptance: same-second and preserved-mtime edits are detected without full rehashing; identical contents converge without conflict; first-sync divergence, restored older versions, delete/recreate and misleading clocks preserve data; one failing or conflicting item no longer stops the rest of the pair.

## Phase 3 — Conflict preservation, Keep both and recovery history

Targets: persistent conflict/recovery entities, `src/sync/model.rs`, `src/sync/mod.rs`, `src/ui/controller.rs`, `qml/Shell.qml`, new recovery helpers.

- Edit/edit and bootstrap conflicts are always resolved automatically by Keep both, matching Syncthing: the version with the older modification time moves to a conflict copy; on equal times the remote version moves. Both versions survive either way. There is no setting to hold these conflicts for review and no Keep local/Keep remote choice; the user resolves them by keeping or deleting the conflict copy like any other file.
- Conflict copies are ordinary visible siblings named `<stem>.sync-conflict-<UTC yyyymmdd-hhmmss>-<local|remote>-<short op id>.<ext>`, which synchronize like any other file. Use no-replace creation, filename-length/character handling for the remote's encoding, and idempotent operation IDs; never overwrite an existing conflict copy or rename the same conflict again on retry. Apply a cap on conflict copies per item (proposed 10, as Syncthing's `maxConflicts`), pruning only verified copies already present on both sides.
- Remove the boolean `ChooseConflict` command and the Keep local/Keep remote dialog. User actions remain only where automation cannot decide: `ConfirmDeletion` after an edit/delete outcome, `Restore` from recovery history, and retry after a blocked collision has been renamed. Bind every action to the pair, item, generation and the content tokens shown. Cancel changes nothing. Reopening refreshes observations; a changed token requires a fresh look.
- Do not mark resolution complete until every resulting version is verified and the operation journal is committed; partial success remains recoverable and retryable.
- File/folder and case collisions require a non-destructive rename or staged replacement that preserves the whole displaced tree. The UI must not offer an unguarded file-copy action for them. First-sync conflicts use neutral “contents differ” wording rather than “both are more recent”.
- Show the reason, paths, kind, size, precise modification times, where each version now lives and the operation outcome. Provide access to conflict/recovery copies and a Restore action. Hashes can remain in diagnostics.
- Add always-on recovery for application-driven overwrites and deletions, separate from visible conflict copies. Local: keep displaced files in the pair's internal recovery directory. Remote: overwrite with `copyfile` plus `_config.BackupDir` pointing at a per-pair recovery path on the same remote outside the synced folder, and delete with `movefile` into that path; verify the backend supports server-side move before relying on it, since a fallback copy costs transfer and quota. Fixed retention: the newest 10 versions per logical item for 30 days. Never automatically prune unresolved conflicts, incomplete operations or the only recoverable copy. Low storage/quota pauses the operation before mutation; it does not silently disable preservation.
- Keep journals/manifests in application configuration storage with restrictive permissions. Local staging/quarantine/recovery lives in a newly created, ownership-marked internal directory inside the pair root (same filesystem; verify `st_dev` matches the destination's), is excluded from synchronization by verified identity, and is never a preexisting user directory. Remote recovery paths are created by Celestial, are outside the pair's remote folder and are documented as visible to other clients of the account. Visible conflict copies obey user exclusions; if exclusions prevent required propagation, explain the incomplete resolution and retain recovery data.
- Restore uses the same guarded pipeline and preserves the current version first. Stopping a pair/removing an account must not silently discard retained recovery data or forget an incomplete operation; retain an inventory with enough original identity/path information to recover it.

Acceptance: each conflict preserves both original contents and converges without user action; Keep both and retries create exactly the intended files; cancellation/restart, permission failures, quota exhaustion and failed history writes retain actionable conflicts.

## Phase 4 — Guarded operations, race handling and crash recovery

Targets: every copy/delete/type-replacement helper and baseline writer in `src/sync/engine.rs`, backend adapters in `src/rclone.rs`, local filesystem helpers and persistent operation journal.

Route normal synchronization, automatic Keep both and user actions through the same operation pipeline:

1. Observe the source and destination; record expected tokens, intended action, recovery paths and operation ID durably.
2. Transfer to a staging name (downloads) or directly with backup-dir preservation (uploads), always with `_config.IgnoreTimes` so rclone cannot skip an intended transfer.
3. Commit against the expected destination token. A changed token becomes a conflict or retry, not an overwrite authorized by the old observation.
4. Verify the committed object and record the source token observed in step 1, the destination's resulting token and the recovery inventory transactionally. If the source's token changed during the operation, leave the item pending; never record later bytes as transferred. This replaces today's post-transfer reread in `push_local`/`push_remote`.
5. Mark the journal complete and only then remove disposable staging. Recovery copies follow retention policy, not temporary-file cleanup.

- Downloads: pass a `dstRemote` inside the internal staging directory so rclone's `.partial` file and the finished file never appear at user paths. Commit a new file with `renameat2(RENAME_NOREPLACE)`. Replace an existing file with `renameat2(RENAME_EXCHANGE)`, then compare the displaced inode with the expected token: if it differs, an edit arrived in the window, so keep it as a conflict copy rather than discarding it. If the filesystem rejects these flags, pause the item rather than falling back to delete-then-copy.
- Uploads: rclone already rejects a source whose size or mtime changes while it is read and verifies a shared hash after transfer; keep both (`IgnoreChecksum` off). Compare the local (size, mtime, ctime, inode) tuple before and after the transfer for the same-size, same-mtime case rclone does not see. Stage a snapshot copy only for remotes without a shared hash type, where rclone cannot verify the bytes.
- Local I/O: resolve paths relative to an open verified root directory, do not follow symlinks, and verify device/inode identity. Flush file and directory state before recording a commit.
- Remote commits are check-then-act; rclone offers no conditional write. Minimize the window by restating the destination (with hash) immediately before the transfer, and make the commit preserving: `BackupDir` moves whatever object is at the destination aside, including an edit from another client that arrived after the check. Detect that case by comparing the backed-up object's token with the expected one and turn it into a conflict copy. Document the residual window (an edit landing between the backup move and the upload's completion) and treat provider version history as extra protection, not a guarantee. Do not pause automatic remote updates on every backend because conditional writes are unavailable; that would disable synchronization everywhere.
- Replace `remove_dir_all` and `operations/purge` with per-item guarded removal into recovery plus empty-directory cleanup (`rmdir`). Preserve descendants that become changed, excluded or untracked after scanning; remote listing failure or delayed consistency cannot authorize recursive deletion.
- Files still being edited through open handles cannot be made globally immutable by Celestial's own lock. Preserve displaced inodes/objects, detect instability, retain recovery paths, and require a stable retry or user review. Do not promise zero races against arbitrary non-cooperating writers.
- On startup, replay incomplete journal states idempotently: locate staged and displaced versions, verify identities, reconcile uncertain commits and keep errors if recovery cannot be proven. Remove or adopt stray `*.partial` files only inside the internal staging directory; report (do not upload) any `.<8 chars>.partial` left at user paths by earlier versions. Cover interruption before/after mutation and before/after the database commit. Database failure after transfer must not discard recovery assets or acknowledge uncertain history.
- Replace silent runtime database fallbacks and absence/error conflation with visible pair/worker errors; make malformed RPC responses and invalid remote names normal errors. Preserve the last known UI state while paused. Ensure stop/quit and account changes cancel/reconcile the scoped operation without performing new writes afterward.

Acceptance: injected failures and edits at every stage preserve all versions observed or displaced by the operation; retries and crashes do not duplicate conflict copies, upload partial files or acknowledge untransferred bytes; same-size/same-mtime transfers are not skipped; the residual remote window is documented and tested.

## Phase 5 — Finish watcher and remote-monitoring hardening

Targets: `src/sync/watch.rs`, `RemoteChecks` and worker scheduling.

- Keep recursive watching installed before initial observation, the root identity check before each pair decision and fallback scanning for overflow, errors, watch limits, network/FUSE filesystems and root replacement. A parent-directory watch is optional: the identity check already detects removal, recreation and remounts on the next loop.
- Debounce write/rename bursts before starting a pass on a dirty pair: a short quiet window (proposed 250 ms) bounded by a maximum delay (proposed 2 s). A continuously written file still requires a stable snapshot or a backoff retry, never a forced unsafe copy.
- Keep the existing guarantee that an event during a scan leaves the pair dirty, and add a regression test for it. Per-path event tracking is not required; change detection relies on the metadata tuple from phase 2, so an event only needs to schedule a scan.
- Exclusion-file changes and shared/nested roots must mark every affected pair.
- Keep the 60-second remote listing: a local watcher cannot see another client's cloud edits and the embedded rclone exposes no change notification. Use `_config.UseListR` where the backend supports recursive listing in one request, and record listing duration so slow remotes back off instead of overrunning the interval. A future rclone upgrade or Go shim exposing change cursors is out of scope.
- Retry blocked items on their backoff schedule (phase 2) and after dismissal, restart or recovery; every retry reobserves the relevant roots and objects before mutating them.

Acceptance: atomic-save/rename storms, root loss/recreation/remount, overflow, exhausted watches, unsupported filesystems and offline recovery never miss permanent changes or propagate a mistaken mass deletion. Remote changes remain discoverable without local activity, and transient failures recover without user action.

## Phase 6 — Bound the added cost and remove existing overhead

Targets: probes/walks, `Shared::publish`, `SnapshotBus`, `src/ui/controller.rs`, `src/ui/tray.rs` and exclusion loading.

- Measure full passes and idle cycles: local walks/hash bytes, remote list/stat/transfer RPC counts, history reads, snapshots, tray updates, memory, CPU and command latency. Repeat the audit's 20,000-file fixture and add larger/nested and no-hash remote fixtures; record reproducible results instead of inheriting historical timings.
- Drive passes from one recursive listing and one history read per pair instead of a `stat` RPC, a history query and an `exists` query per item. Fresh destructive preconditions still override cached observations. Cache fingerprints by (size, mtime, ctime, inode) and exclusions by file identity; unknown reads remain errors.
- Coalesce identical JSON snapshots and tray message/phase updates before publishing. Read exclusions when they change instead of in every `snapshot`. Throttle ordinary progress publication (proposed 100–250 ms), while errors, conflict state, stop and quit remain immediate. Worker commands and notices must not be suppressed by deduplication.
- Bound hashing/transfer work and support cancellation between files so hashing large files does not starve the Qt UI, tray, other pairs or stop commands. Measure no-op behavior and command latency as well as active synchronization; keep remote scheduling bounds under ordinary load and report overruns.

Acceptance: a stable idle pair rereads no file contents, remote reconciliation retains its schedule, identical UI/tray state is not republished, and added verification costs are measured and documented.

## Phase 7 — Required validation and delivery gates

Tests below are future implementation work. Do not run them or change runtime configuration as part of approving this plan.

| Gate | Required coverage and evidence |
| --- | --- |
| Reconciliation and migration | G01–G03/G09/G16: same-second and same-size/preserved-mtime edits on each/both sides (ctime path and provider-hash path), racy-clean files, clock skew/ties, identical concurrent edits, older restores, post-2038/pre-epoch times, first sync, missing/corrupt/legacy baseline, tombstones, delete/recreate, directory convergence, account isolation, duplicate migration, one failing item while others sync, backoff retry. Verify contents and persisted state, not only error counts. |
| Preservation and UI | G04–G06/G11: automatic Keep both (edit/edit and bootstrap), edit/delete confirmation, cancellation, stale content token, name/case/normalization collisions, non-UTF-8 names, long names, file/folder conflicts, conflict-copy cap, exclusions, full disk/quota, restore, restart and idempotent retries. Exercise the real QML controls with disposable data and verify files after each action. |
| Operation faults and concurrency | G07–G08/G15: deterministic failpoints/barriers during source read, staging, preservation, commit, verification and DB write; destination edits inside the commit window (local exchange and remote backup-dir paths); path-to-symlink swaps; changed descendants; root swaps; stop/quit; malformed RPCs; database enumeration/read/write failures; disk-full and truncated transfers; crash with a `.partial` file present; `copyfile` against a same-size, same-mtime destination. Verify old/new/displaced bytes and recovery journal after crash/restart. |
| Backend capabilities | G10/G14: local aliases plus isolated fixtures for a no-hash backend, a coarse-mtime backend, a backend without server-side move, duplicate names, a case-insensitive backend and delayed/incomplete listing. Unsupported capabilities must exercise the documented fallback rather than skip the case. |
| Live provider behavior | Dedicated disposable roots on each provider Celestial offers at login (Drive, Dropbox, OneDrive, pCloud, Proton Drive, Nextcloud/ownCloud, generic WebDAV). Validate interrupted upload/download, recoverable deletion and overwrite, type transitions and concurrent edits from two clients/Celestial instances. No existing user sync folder/account is part of the fixture. Without available test credentials, keep this gate unverified and do not claim production validation. |
| Watchers and performance | G12–G13: atomic rename saves, burst debounce/deadline, overflow, events during scans, watch-limit/error recovery, missing/recreated/remounted root, network/FUSE fallback, shared roots, offline reconnect, scheduled remote discovery and full metadata scans. Measure large-tree idle/active CPU, bandwidth/RPCs and UI/stop latency. |
| Existing behavior | Retain every repaired audit regression: scoped single-file deletion, child traversal, rules-file protection, failed copy history, missing/empty roots, excluded/untracked descendants, pair removal transactions, path confinement and startup errors. Validate tray/background operation and legacy config migration with disposable XDG directories. |
| Release and portable bundle | Fresh remotely built executable, regenerated portable bundle, extraction/cache reuse/library closure/headless QML startup, then actual desktop/tray/conflict workflow and restart recovery. Exercise container bundling too. Record the actual ABI floor; the old audit's glibc 2.39 result is historical. Ubuntu 22.04 packaging cannot lower the binary's ABI: verify a clear refusal when incompatible, and successful bundling/runtime on a compatible environment. Do not claim Ubuntu 22.04 runtime support without a compatible remote toolchain and real test. |

Rust validation follows `AGENTS.md` and `BUILDING.md`, using the configured private builder. No local Rust build/test/Clippy fallback and no forwarded `-- -D warnings`:

```sh
cargo fmt --check
cargo remote -h -- test --locked
cargo remote -h -- clippy --locked --all-targets
bash scripts/build-run.sh
rm -rf target/debug
bash -n scripts/*.sh scripts/lib/*.sh
git diff --check
```

`-h` transfers `.cargo/config.toml`, which selects the `lld` linker cxx-qt needs. Use `scripts/build-native.sh --release`, `scripts/build-portable.sh` and `scripts/smoke-portable.sh` for the portable gates; use the resulting artifact path/version rather than a hard-coded old release. Read `~/.config/cargo-remote/cargo-remote.toml` and the `rust-builder` entry in `~/.ssh/config` directly when checking the builder. A remote failure is reported, not worked around with local compilation.

After every implementation commit, immediately run a fresh `bash scripts/build-run.sh` and remove `target/debug`, as AGENTS.md requires. Stop and report any release failure before committing more work. There is no commit in the planning task.

## Delivery order and completion criteria

Implement in phases 1–7 after separate implementation approval. Two fixes are small and independent and can land first: forcing intended transfers with `_config.IgnoreTimes` (G01) and replacing the pair-wide halt with per-item blocking (G16). Phase 2 may land as a tested decision module before it is connected to live mutations; enable the new mutation behavior only after preservation and guarded operations from phases 3–4 are ready. Watcher/performance work must preserve the new verification guarantees.

- [ ] Every G01–G16 item has implementation and acceptance evidence, including the documented behavior for each backend capability.
- [ ] Every pending audit item has a recorded outcome; repaired items still pass their regressions. Historical metrics are replaced by measured current results where relevant.
- [ ] Bootstrap/migration never invents a common baseline, discards divergence or interprets a failed observation as deletion.
- [ ] Conflict resolution, normal replacement, deletion, type replacement and restore all use the same preservation/commit/recovery path.
- [ ] One failing or conflicting item never stops the rest of its pair; transient failures retry automatically.
- [ ] No silent destructive fallback exists; capability limits, the residual remote race window and recovery locations are visible and documented.
- [ ] Live provider, real desktop/tray, crash recovery and compatible portable delivery gates are satisfied for supported claims. Unavailable evidence remains explicitly incomplete.
- [ ] README/BUILDING document the resulting conflict, deletion, retention, restore and backend behavior without presenting sync as a backup of arbitrary local edits.

Additional references: [rclone list metadata and hashes](https://rclone.org/rc/#operations-list-list-the-given-remote-and-path-in-json-format), [rclone copyfile interface](https://rclone.org/rc/#operations-copyfile-copy-a-file-from-source-remote-to-destination-remote), [rclone filesystem information](https://rclone.org/rc/#operations-fsinfo-return-information-about-the-remote), [rclone backup-dir](https://rclone.org/docs/#backup-dir-dir), [notify caveats](https://docs.rs/notify/8.2.0/notify/#known-problems), [racy Git](https://git-scm.com/docs/racy-git), [Syncthing conflict-copy retention](https://docs.syncthing.net/users/config.html#maxconflicts), [separate historical versioning](https://docs.syncthing.net/users/versioning.html).
