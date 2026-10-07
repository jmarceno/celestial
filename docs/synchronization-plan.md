# Synchronization reliability implementation plan

Date: 2026-10-07. Reviewed checkout: `2a970e1`.

Status: planning only; none of the changes below are implemented by this document. This plan replaces `docs/synchronization-audit.md`, whose original evidence remains in Git history. Current status was checked against source and existing test definitions; builds, tests, cloud accounts and running application data were not accessed during this review. Earlier test counts and timings are historical evidence, not fresh validation. Keep plans and audits under `docs`; do not link them from README or AGENTS.md.

## Goal and scope

Detect content changes reliably, preserve recoverable versions before replacement or deletion, and prevent stale observations from authorizing destructive operations. Cover every outstanding item from the audit and the Syncthing comparison while retaining the existing Rust worker, Qt Quick UI, rclone integration, account isolation, exclusions and tray behavior.

Syncthing tracks participating devices with version vectors and uses content hashes to distinguish conflicting contents. Celestial synchronizes a local folder with arbitrary cloud storage, including edits from clients that do not publish such vectors. Use a last-common-content baseline, per-pair generations and provider revisions to distinguish independent changes. These generations order Celestial's own operations; they are not distributed version vectors. Never use wall-clock ordering to decide which conflicting content is authoritative. [Syncthing protocol](https://docs.syncthing.net/specs/bep-v1.html#fields-fileinfo-message), [conflict handling](https://docs.syncthing.net/users/syncing.html#conflicting-changes).

## Existing audit: disposition

“Present” below means the repair exists in the inspected code and relevant tests exist where listed. It does not mean those tests were rerun or that every cloud backend has been validated. Preserve these fixes as regressions throughout implementation.

| Audit finding | Current evidence and status | Work in this plan |
| --- | --- | --- |
| Single-file deletion used a filesystem-wide operation | Present: `src/rclone.rs` uses `operations/deletefile`; `deleting_one_remote_file_preserves_neighbors_and_other_folders` | Retain; add guarded deletion in phase 4. |
| Normal file copies deleted the destination first | Ordinary same-type copies no longer remove it; missing-source failure tests exist. Mid-transfer behavior remains unverified. | Staging, recovery and provider failure tests in phases 3–4 and 7. |
| Unchanged parent timestamps hid changed children | Both walks recurse independently; local/remote child-edit tests exist. | Retain; content checks in phase 2. |
| History/conflict/stop queries crossed account or pair boundaries | Scoped queries and cross-remote tests are present. | Extend the same isolation to fingerprints, journals, conflicts and recovery stores in phase 1. |
| Stop/remove changed the UI before database success | `process_deletions` uses transactions; failure tests exist. | Retain; integrate pending operation recovery in phase 4. |
| Failed deletion lost history; updates inserted duplicates | Success-dependent removal and baseline updates are present; failure/duplicate tests exist. | Retain; journal commit protocol in phase 4. |
| Missing or empty roots caused mass deletion | Availability checks and empty-root guards are present; both directions have tests. | Retain; root identity and destructive-operation checks in phases 4 and 7. |
| Recursive deletion trusted only the parent | Descendant guards and changed-child tests are present. Validation and deletion still race. | Content-aware quarantine and guarded deletion in phase 4. |
| Exclusions used inconsistent paths or synchronized the rules file | Shared root-relative matching, rule-read failures and local-only `.sync-exclude.lst` protection are present. | Retain; internal recovery paths and exclusions tests in phases 3 and 7. |
| Download baselines used pre-transfer metadata; write errors disappeared | Actual destination timestamps and visible baseline-write errors are present. No proof that both sides still match the transferred bytes. | Verified content baselines and recovery in phases 2 and 4. |
| Traversal paths or symlinks escaped the pair | `local_path_for_remote`, `validate_local_path` and regression tests are present. Path validation is separate from I/O. | Descriptor-based local I/O and path-swap tests in phase 4. |
| Startup errors were discarded or mislabeled | Worker startup errors have a specific fatal dialog in `spawn`. | Preserve; test alongside visible runtime observation failures in phase 7. |
| Whole-second timestamps miss edits | Still pending: probes, transfers and `ItemVersion` use whole-second timestamps; `RcloneRemoteItem` omits size/hash/revision. | Phases 1–2. |
| Conflict choice can become stale during copying | Timestamp/type revalidation is present; the check-to-copy race remains. | Phases 3–4. |
| File/folder replacement can remove the old item before a failing transfer | Still pending in `push_local`, `pull_local`, `push_remote` and `pull_remote`. | Phase 4. |
| Watchers, fallback reconciliation and scheduled remote checks | Implemented in `src/sync/watch.rs` and `RemoteChecks`: recursive watch before scanning, root identity checks, unsupported-filesystem/error fallback, 10-minute full scans, 60-second idle remote checks. | Retain; missing debounce/parent watch and validation in phase 5. |
| Frequent per-file RPCs, snapshots and tray updates | Still present; snapshots also reread exclusion files. | Phase 6. |
| Live cloud behavior, production efficiency and packaging-container path | Earlier validation used temporary local aliases. Cloud behavior and Ubuntu 22.04 container bundling were not exercised. | Explicit gates in phase 7. |
| Post-2038 timestamps, restored older versions and duplicate history | 64-bit fields, comparisons in both timestamp directions and `m20261007_000001_sync_items_unique_paths` are present with tests. | Keep existing migrations unchanged; preserve these cases in phases 1–2 and 7. |
| Deprecated components and branding/config migration | Prior removal and migration are already represented in the checkout; no synchronization work remains on that cleanup. | Preserve active OAuth/assets, legacy config migration and portable delivery in phase 7. |
| Suspected failure to converge folder baselines | Not reproduced by the original audit; do not label it a confirmed defect. | Retain a convergence regression in phase 7. |

The old audit's proposed watcher implementation and statements that idle pairs always traverse both trees are superseded by the current watcher and remote schedule. Remaining watcher work is hardening and validation, not reinstalling the existing mechanism.

## Complete gap register

| ID | Gap or outstanding validation | Planned closure |
| --- | --- | --- |
| G01 | Same-second, preserved-mtime, backwards-clock and coarse-provider timestamp handling | Precise metadata, fingerprints and independent integrity sweeps: phases 1–2. |
| G02 | Both timestamps change but bytes are identical | Compare contents and advance history without a conflict: phase 2. |
| G03 | First sync or lost/legacy history selects by timestamp, including remote-on-tie | Safe bootstrap with explicit conflicts for differing contents: phases 1–2. |
| G04 | Keep local/remote destroys the other version; no Keep both or durable conflict record | Protected conflict copies, persistent records and all three choices: phase 3. |
| G05 | No recoverable history for ordinary replacements and propagated deletions | Recovery inventory, restore and explicit retention: phases 3–4. |
| G06 | Prompt revalidation cannot detect content changes that preserve metadata | Bind choices to content/revision tokens and reject stale tokens: phases 2–4. |
| G07 | Source, destination, descendants, paths and roots can change after observation; baselines can acknowledge bytes never transferred | Stable staging, guarded commits, journal recovery and verified baselines: phase 4. |
| G08 | File/folder replacement removes the old destination before success | Stage and quarantine both replacement directions: phase 4. |
| G09 | Edit/delete and delete/recreate lack an explicit persistent reconciliation policy | Presence states, tombstones and content-aware decisions: phases 1–4. |
| G10 | Missing hashes/revisions, conditional writes, atomic moves or ChangeNotify vary by backend and linked rclone version | Verified capability adapters and safe unsupported-backend behavior: phases 1, 4–5 and 7. |
| G11 | Case/name collisions and duplicate remote names can make conflict/staging paths ambiguous | Collision checks, object identities and safe naming: phases 1–4. |
| G12 | Atomic-save bursts, watcher overflow/root lifecycle and offline recovery have incomplete coverage | Debounce, parent watch, reconciliation and fault tests: phases 5 and 7. |
| G13 | Avoidable remote work, JSON generation, exclusion reads and tray updates | Instrumentation, safe caching and coalescing: phase 6. |
| G14 | Local alias tests/headless startup do not establish live cloud, failure or desktop behavior | Provider, crash, UI, performance and portable gates: phase 7. |
| G15 | Runtime database enumeration failures can look like empty configuration; malformed RPC JSON can panic | Explicit observation errors and fallible decoding: phases 1, 4 and 7. |

G15 was also confirmed during this review: `worker`/`reload_model` use `unwrap_or_default`, `SyncDirsModel::exists` collapses errors into absence, and RPC response decoding uses `unwrap`. Address these as part of uncertain-state handling rather than treating failures as successful empty observations.

## Phase 1 — Versioned observations and persistent state

Targets: `src/entities/sync_items.rs`, new additive migrations/entities, `src/migrations/mod.rs`, `src/rclone.rs`, `src/sync/model.rs`; introduce focused observation/reconciliation modules under `src/sync`.

- Define observations with explicit `Present`, `Missing` and `Unknown/Error` states. A failed/incomplete read is never evidence of deletion. Store file/folder kind, size, modification time as signed seconds plus nanoseconds, provider timestamp precision, and local device/inode/ctime for invalidation. Keep timestamps after 2038 and before the Unix epoch representable.
- Store content fingerprints with algorithm and verification provenance; retain per-side object/revision tokens separately. Object IDs and ETags are not automatically content hashes. Only compare hashes with the same algorithm and verified semantics; never interpret a missing hash as empty-file contents.
- Persist the last common verified contents, last verified observations, pair generation, deletion tombstones, pending conflicts and operation/recovery records. Keep `(sync_dir_id, local_path, remote_path)` uniqueness and a stable pair identity for recovery storage. Tombstones record confirmed deletion, not listing failure; retain them through offline/retry periods and until reconciliation is acknowledged.
- Add migration versioning and a safe database backup/restore procedure. Keep old migrations intact. Existing timestamp-only rows become `NeedsVerification`, never invented content baselines. If current sides differ and the former common contents cannot be established, preserve both and require review; a missing side during this bootstrap must not authorize deletion of the other. Both sides absent is retired only after complete successful observations. Version journal payloads and reject unknown formats; document downgrade as requiring a matching database backup and preservation of recovery assets, rather than running an older timestamp-only binary against new operation state.
- Record backend capabilities using `operations/fsinfo` and verified behavior: hash algorithms, precision, case sensitivity, duplicate names, object identity, move semantics, conditional create/replace/delete, version retention and change notifications. Do not infer conditional writes from `Move` or server-side `Copy` support.
- Verify the actual linked rclone runtime through `core/version` and RPC discovery during future isolated tests. `Cargo.toml` pins `librclone` 0.6.3; the old audit reported rclone 1.64.2. Modern documentation is not proof that this embedded runtime exposes an API. Any necessary dependency/adapter change must be explicit, locked and included in the remote test/release gates.

Acceptance: old databases migrate without erasing history; unknown observations cannot trigger transfer/deletion; two accounts with identical paths remain independent; unsupported capabilities are represented explicitly. [rclone filesystem information](https://rclone.org/rc/#operations-fsinfo-return-information-about-the-remote).

## Phase 2 — One content-aware reconciliation algorithm

Targets: the local/remote probes and walks in `src/sync/engine.rs`, observation/reconciliation modules, `src/rclone.rs`, `src/sync/watch.rs`.

Replace duplicated timestamp decision trees with one decision function over both observations and the verified common baseline. Metadata and watcher events schedule work; content identity decides whether contents changed. Always freshly verify both sides before an operation that could replace/delete an existing item, even when destination metadata appears unchanged.

| Observed state | Required decision |
| --- | --- |
| No trusted common baseline; both regular files have equal contents | Establish a verified common baseline without copying. |
| No trusted common baseline; both exist with different contents or kinds | Record a bootstrap conflict; leave both untouched. Timestamp ties and clock skew do not select a winner. |
| No history at all; only one side exists | Create the missing copy with collision-safe creation after successful root/absence checks. Legacy or damaged history uses the more conservative bootstrap rule from phase 1. |
| Both equal the common contents | No content transfer. Refresh verified metadata/revision tokens as needed. |
| Exactly one side differs from the common contents | Propagate that version through the guarded transfer protocol. |
| Both changed to identical contents | Advance the common baseline without a conflict or redundant upload. |
| Both changed to different contents | Persist an edit/edit conflict and preserve both until resolution. |
| One deleted; surviving contents equal the verified baseline | Propagate deletion through recoverable, guarded removal. |
| One deleted; surviving contents changed | Record edit/delete conflict. Default resolution preserves/restores the edit; an explicit delete choice first archives it. |
| Both confirmed absent | Acknowledge the tombstone and retire active history only after complete observations; retain recovery data according to policy. |
| Any observation uncertain or the item is still changing | Retry/pause visibly; do not advance history. |

- Hash local files when a write/rename event occurs, when size/precise metadata/identity changes, during bootstrap and before destructive operations. A watcher event must invalidate the fingerprint even when the file's final size and mtime are unchanged.
- Use validated provider content hashes when available, computing the matching local algorithm. Otherwise stream/download to controlled staging and compute SHA-256. For mutable inputs, validate identity/revision before and after reading; a moving target is not a verified snapshot.
- Add rolling content-integrity sweeps independent of metadata and watcher hints. Proposed default: every tracked regular file verified within 24 hours, with configurable bandwidth and deadlines. Keep the existing 10-minute metadata reconciliation and 60-second remote scheduling separately. Record per-file verification age, make overdue/unavailable verification visible, and never imply unchanged metadata proves unchanged bytes. No-hash remotes may require reading the entire file; document that cost.
- Reject ambiguous mappings before mutations: case-insensitive collisions, normalization collisions and multiple remote objects at the same logical path. Preserve raw names/IDs, avoid silently deduplicating and offer rename/review guidance. Directory comparisons use descendants, not directory mtime or a file hash.

Acceptance: same-second and preserved-mtime edits are caught by events/sweeps and always before destructive actions; identical contents converge without conflict; first-sync divergence, restored older versions, delete/recreate and misleading clocks preserve data.

## Phase 3 — Conflict preservation, Keep both and recovery history

Targets: persistent conflict/recovery entities, `src/sync/model.rs`, `src/sync/mod.rs`, `src/ui/controller.rs`, `qml/Shell.qml`, new recovery helpers.

- Replace the boolean conflict command with explicit `KeepLocal`, `KeepRemote`, `KeepBoth`, `PreserveEdit` and `ConfirmDeletion` actions as appropriate. Bind every action to the pair, conflict ID, generation and reviewed content/revision tokens. Cancel keeps the conflict unresolved. Reopening refreshes observations; a changed token requires review again.
- Before resolving edit/edit, preserve and verify the losing version. Create an ordinary visible sibling conflict file with the original extension, a UTC timestamp, side/instance ID and unique operation suffix. It synchronizes like any other user file. Use collision-safe creation, filename-length/character handling and idempotent operation IDs; never overwrite an existing conflict copy or recursively rename the same conflict on retry.
- Keep both leaves the local version at the original logical path and puts the remote version at a generated sibling path on both sides. Explain this in the dialog. Do not mark resolution complete until both versions are verified and the operation journal is committed; partial success remains recoverable and retryable.
- File/folder and case collisions require a non-destructive rename or staged replacement that preserves the whole displaced tree. The UI must not offer an unguarded file-copy action for them. First-sync conflicts use neutral “contents differ” wording rather than “both are more recent”.
- Show the reason, paths, kind, size, precise modification times, preservation destination and operation outcome. Provide access to conflict/recovery copies and a Restore action. Hashes/revision details can remain in diagnostics; the user needs to understand which data will survive.
- Add always-on recovery for application-driven overwrites and deletions, separate from visible conflict copies. Proposed normal-history retention: the newest 10 versions per logical item for 30 days, configurable per pair. Never automatically prune unresolved conflicts, incomplete operations or the only recoverable copy. Low storage/quota pauses the operation before mutation; it does not silently disable preservation.
- Keep journals/manifests in application configuration storage with restrictive permissions. Local staging/quarantine must be on the destination filesystem for atomic operations; use newly created, ownership-marked internal directories and ignore only those verified application-owned paths. Never claim or hide a preexisting user directory. Remote staging/recovery also uses verified unique paths. Visible conflict copies continue to obey user exclusions; if exclusions prevent required propagation, explain the incomplete resolution and retain recovery data.
- Restore uses the same guarded pipeline and preserves the current version first. Stopping a pair/removing an account must not silently discard retained recovery data or forget an incomplete operation; retain an inventory with enough original identity/path information to recover it.

Acceptance: each resolution preserves both original contents; Keep both and retries create exactly the intended files; cancellation/restart, permission failures, quota exhaustion and failed history writes retain actionable conflicts.

## Phase 4 — Staged operations, race handling and crash recovery

Targets: every copy/delete/type-replacement helper and baseline writer in `src/sync/engine.rs`, backend adapters in `src/rclone.rs`, local filesystem helpers and persistent operation journal.

Route normal synchronization and manual resolution through the same operation pipeline:

1. Observe and fingerprint the source and destination; record expected tokens, scope, intended action, recovery paths and operation ID durably.
2. Stage the exact source bytes under a unique name. Verify complete content and source stability. Preserve the existing destination/tree before destructive commitment.
3. Commit using verified local/provider semantics against the expected destination token. A changed token becomes a new conflict or retry, not an overwrite authorized by the old observation.
4. Verify the committed object and record the exact transferred fingerprint, destination revision and recovery inventory transactionally. If the source changed meanwhile, leave the pair dirty; never acknowledge its new bytes as already transferred.
5. Mark the journal complete and only then remove disposable staging. Recovery copies follow retention policy, not temporary-file cleanup.

- For local I/O, resolve paths relative to an open verified root directory, do not follow symlinks, and verify device/inode identity. Stage on the same filesystem, flush file/directory state, and atomically retain the actual displaced destination through quarantine/exchange semantics. A separate `stat` followed by rename is not a compare-and-swap. Validate displaced contents against the expected token, preserve unexpected contents and recover/rollback without overwriting intervening edits.
- Ordinary downloads must never stream into the live destination. Upload from a stable staged snapshot. Local creation must use no-replace semantics; replacement and file/folder transitions retain the old file/tree until the new one is verified. If the filesystem cannot provide the required operation, pause rather than falling back to delete-then-copy.
- For remote commits, use conditional create/replace/delete tied to verified revisions when exposed by the backend adapter. Generic rclone `copyfile` parameters do not establish this guarantee. Verified server-side version retention can provide a recovery guarantee for intervening versions, but is not itself compare-and-swap; label and test that distinction.
- If a backend offers neither conditional commitment nor proven preservation of intervening versions, do not automatically replace/delete an existing remote path. Preserve originals, create a verified new sibling where safe, and surface a paused/manual-resolution state. A last-second metadata check is insufficient. Implement the capability path explicitly; never advertise all rclone remotes as equally race-safe.
- Replace blanket recursive purge with per-item guarded removal plus empty-directory cleanup, or a proven atomic tree-quarantine operation. Preserve descendants that become changed, excluded or untracked after scanning; remote listing failure or delayed consistency cannot authorize recursive deletion.
- Files still being edited through open handles cannot be made globally immutable by Celestial's own lock. Preserve displaced inodes/objects, detect instability, retain recovery paths, and require a stable retry or user review. Do not promise zero races against arbitrary non-cooperating writers.
- On startup, replay incomplete journal states idempotently: locate staged and displaced versions, verify identities, reconcile uncertain commits and retain errors if recovery cannot be proven. Cover interruption before/after mutation and before/after the database commit. Database failure after transfer must not discard recovery assets or acknowledge uncertain history.
- Replace silent runtime database fallbacks and absence/error conflation with visible pair/worker errors; make malformed RPC responses normal errors. Preserve the last known UI state while paused. Ensure stop/quit and account changes cancel/reconcile the scoped operation without performing new writes afterward.

Acceptance: injected failures and edits at every stage preserve all versions observed or displaced by the operation; retries and crashes do not duplicate conflict copies or acknowledge untransferred bytes. Unsupported destructive guarantees result in an explicit safe fallback.

## Phase 5 — Finish watcher and remote-monitoring hardening

Targets: `src/sync/watch.rs`, `RemoteChecks` and worker scheduling.

- Keep recursive watching installed before initial observation and retain fallback polling for overflow, errors, watch limits, network/FUSE filesystems and root replacement. Add a shared nonrecursive parent watch for root removal/recreation; keep device/inode polling because parent events alone do not prove mount health.
- Debounce write/rename bursts with a short quiet window (proposed 250 ms) and a bounded maximum delay (proposed 2 s). A continuously written file still requires a stable snapshot or visible retry, never a forced unsafe copy. Track event generations during scans so events arriving mid-scan cannot certify the pair clean.
- Events invalidate relevant fingerprints and integrity deadlines. Celestial's own writes still cause a verification; reads/listings do not. Shared/nested roots and exclusion changes must invalidate every affected pair correctly.
- Preserve scheduled remote reconciliation: a local watcher cannot detect another client's cloud edits. Evaluate `ChangeNotify` only where the actual embedded library can expose a usable subscription/cursor. A feature flag alone is insufficient; absent support retains 60-second checks. Reconnect/cursor loss forces a complete observation and does not erase tombstones/history.
- Keep availability errors and conflicts visible; retries after dismissal, restart or recovery must reobserve the relevant roots and objects before mutating them.

Acceptance: atomic-save/rename storms, root loss/recreation/remount, overflow, exhausted watches, unsupported filesystems and offline recovery never miss permanent changes or propagate a mistaken mass deletion. Remote changes remain discoverable without local activity.

## Phase 6 — Bound the added cost and remove existing overhead

Targets: probes/walks, `Shared::publish`, `SnapshotBus`, `src/ui/controller.rs`, `src/ui/tray.rs` and exclusion loading.

- Measure full passes and idle cycles: local walks/hash bytes, remote list/stat/hash/transfer RPC counts, history reads, snapshots, tray updates, memory, CPU and command latency. Repeat the audit's 20,000-file fixture and add larger/nested and no-hash remote fixtures; record reproducible results instead of inheriting historical timings.
- Reuse one complete snapshot and one history read per pair/pass where valid; remove avoidable per-item RPCs and quadratic work. Fresh destructive preconditions still override cached observations. Cache fingerprints and exclusions by verified identity/event generation; unknown reads remain errors.
- Coalesce identical JSON snapshots and tray message/phase updates before publishing. Cache exclusions instead of rereading them for every snapshot. Throttle ordinary progress publication (proposed 100–250 ms), while errors, conflict state, stop and quit remain immediate. Worker commands and notices must not be suppressed by deduplication.
- Limit integrity sweep/transfer work and support cancellation between chunks so hashing large files does not starve the Qt UI, tray, other pairs or stop commands. Measure no-op behavior and command latency as well as active synchronization; keep remote scheduling bounds under ordinary load and report overruns.

Acceptance: a stable idle pair has no repeated tree/content walk outside scheduled verification, remote reconciliation retains its schedule, identical UI/tray state is not republished, and added verification costs are measured and documented.

## Phase 7 — Required validation and delivery gates

Tests below are future implementation work. Do not run them or change runtime configuration as part of approving this plan.

| Gate | Required coverage and evidence |
| --- | --- |
| Reconciliation and migration | G01–G03/G09: same-second and same-size/preserved-mtime edits on each/both sides, clock skew/ties, identical concurrent edits, older restores, post-2038/pre-epoch times, first sync, missing/corrupt/legacy baseline, tombstones, delete/recreate, directory convergence, account isolation and duplicate migration. Verify contents and persisted state, not only error counts. |
| Preservation and UI | G04–G06/G11: all choices, cancellation, stale content/revision token, name/case collisions, long names, file/folder conflicts, protected copies, exclusions, full disk/quota, restore, restart and idempotent retries. Exercise the real QML controls with disposable data and verify files after each action. |
| Operation faults and concurrency | G07–G08/G15: deterministic failpoints/barriers during source read, staging, preservation, commit, verification and DB write; destination edits/path-to-symlink swaps; changed descendants; root swaps; stop/quit; malformed RPCs; database enumeration/read/write failures; disk-full and truncated transfers. Verify old/new/displaced bytes and recovery journal after crash/restart. |
| Backend capabilities | G10/G14: local aliases plus isolated capability fixtures for a revision/conditional-write backend, a no-hash backend, a coarse-mtime backend, versioned objects, non-atomic moves, duplicate names and delayed/incomplete listing. Unsupported real capabilities must exercise the safe fallback rather than skip the case. |
| Live provider behavior | Dedicated disposable roots on each backend claimed to support safe automatic replacement/deletion, with real native capabilities. Validate interrupted upload/download, deletion, type transitions and concurrent edits from two clients/Celestial instances. No existing user sync folder/account is part of the fixture. Without available test credentials, keep this gate unverified and do not claim production validation. |
| Watchers and performance | G12–G13: atomic rename saves, burst debounce/deadline, overflow, event races, watch-limit/error recovery, missing/recreated/remounted root, network/FUSE fallback, shared roots, offline reconnect, scheduled remote discovery and full metadata/content sweeps. Measure large-tree idle/active CPU, bandwidth/RPCs and UI/stop latency. |
| Existing behavior | Retain every repaired audit regression: scoped single-file deletion, child traversal, rules-file protection, failed copy history, missing/empty roots, excluded/untracked descendants, pair removal transactions, path confinement and startup errors. Validate tray/background operation and legacy config migration with disposable XDG directories. |
| Release and portable bundle | Fresh remotely built executable, regenerated portable bundle, extraction/cache reuse/library closure/headless QML startup, then actual desktop/tray/conflict workflow and restart recovery. Exercise container bundling too. Record the actual ABI floor; the old audit's glibc 2.39 result is historical. Ubuntu 22.04 packaging cannot lower the binary's ABI: verify a clear refusal when incompatible, and successful bundling/runtime on a compatible environment. Do not claim Ubuntu 22.04 runtime support without a compatible remote toolchain and real test. |

Rust validation must follow `AGENTS.md` and `BUILDING.md`, using the configured private builder. No local Rust build/test/Clippy fallback and no forwarded `-- -D warnings`:

```sh
cargo fmt --check
cargo remote -h -- test --release --locked
cargo remote -h -- clippy --release --locked --all-targets
bash scripts/build-run.sh
bash -n scripts/*.sh scripts/lib/*.sh
git diff --check
```

Use `scripts/build-native.sh --release`, `scripts/build-portable.sh` and `scripts/smoke-portable.sh` for the portable gates; use the resulting artifact path/version rather than a hard-coded old release. Dotfiles are included above because `.cargo/config.toml` supplies the linker. Read `~/.config/cargo-remote/cargo-remote.toml` and the `rust-builder` entry in `~/.ssh/config` directly when checking the builder. A remote failure is reported, not worked around with local compilation.

If a later implementation commit is explicitly authorized, immediately run a fresh `bash scripts/build-run.sh` and remove `target/debug`, as required after every commit. Stop and report any release failure before committing more work. There is no commit in the planning task.

## Delivery order and completion criteria

Implement in phases 1–7 after separate implementation approval. Phase 2 may land as a tested decision module before it is connected to live mutations; enable the new mutation behavior only after preservation and guarded operations from phases 3–4 are ready. Watcher/performance work must preserve the new verification guarantees.

- [ ] Every G01–G15 item has implementation and acceptance evidence, including an explicit unsupported-backend fallback where required.
- [ ] Every pending audit item has a recorded outcome; repaired items still pass their regressions. Historical metrics are replaced by measured current results where relevant.
- [ ] Bootstrap/migration never invents a common baseline, discards divergence or interprets a failed observation as deletion.
- [ ] Conflict resolution, normal replacement, deletion, type replacement and restore all use the same preservation/commit/recovery path.
- [ ] No silent destructive fallback exists for a backend lacking required guarantees; capability limits, verification age and recovery locations are visible.
- [ ] Live provider, real desktop/tray, crash recovery and compatible portable delivery gates are satisfied for supported claims. Unavailable evidence remains explicitly incomplete.
- [ ] README/BUILDING document the resulting conflict, deletion, retention, restore, integrity-sweep and backend behavior without presenting sync as a backup of arbitrary local edits.

Additional references: [rclone list metadata and hashes](https://rclone.org/rc/#operations-list-list-the-given-remote-and-path-in-json-format), [rclone copyfile interface](https://rclone.org/rc/#operations-copyfile-copy-a-file-from-source-remote-to-destination-remote), [notify caveats](https://docs.rs/notify/8.2.0/notify/#known-problems), [Syncthing conflict-copy retention](https://docs.syncthing.net/users/config.html#maxconflicts), [separate historical versioning](https://docs.syncthing.net/users/versioning.html).
