# Synchronization progress, space check and recent files

Date: 2026-10-08. Embedded rclone: v1.64.2 through `librclone 0.6.3`. Keep plans under `docs`; do not link them from README or AGENTS.md.

## Goal

Show how much data a pass will transfer, the transfer speed and the time left, per pair in the main window and in total from the tray. List recently synchronized files. Warn before a large first sync that does not fit the remote's free space.

## What rclone provides

- Celestial does not run `sync/sync`. A pass decides per item in `src/sync/pass.rs` and copies each file with a separate, blocking `operations/copyfile` call. rclone's own `totalBytes`/`eta` therefore only describe the current file; pass totals come from Celestial's decisions.
- Every RPC accepts `_group`; librclone routes synchronous calls through `jobs.NewJob`, which applies it (`fs/rc/jobs/job.go`, `getGroup`). `core/stats {group}` returns `bytes` and a `transferring` list with per-file `bytes`, `size` and `speed`. `core/stats-delete` frees the group.
- `librclone::rpc` takes no lock and Go serves concurrent cgo calls, so a second thread may poll `core/stats` while the worker blocks in `copyfile`.
- `operations/about` reports remote free space (`free_space`). `operations/size` exists for remote totals.
- `core/transferred` is in memory and capped; recent files are kept in Celestial's database instead.

## Design

1. **Pass plan.** After deciding, sum planned uploads, downloads, Keep both copies and removals with sizes from the observations. Files with an unknown size (`-1`) are counted separately. Report it through `Hooks::plan`. While scanning, report the number of items and bytes found.
2. **Space gate.** When planned upload bytes plus a margin exceed the remote's free space, block the pass before any transfer, as a pair issue: "Needs X, Y free". The person can choose *Sync anyway* (approved until the plan grows past what was approved) or stop the pair. `check_space` stays as the per-file guard.
3. **Byte progress.** `transfer` passes `_group: "pair-<id>"`. A meter thread polls `core/stats` about once a second during a pass and combines it with the plan: done bytes are completed files plus the in-flight bytes. Speed is a moving average over wall-clock time (so per-file overhead is included); time left is remaining bytes divided by that speed. The group is deleted after the pass.
4. **Publication.** Live progress is kept in shared memory and exposed as a separate `progress_json` property refreshed by `tick()`, so the full snapshot is not rebuilt every second. The total sums all pairs.
5. **Recent activity.** A new `sync_activity` table records successful uploads, downloads, deletions, conflict copies and restores with size and time; it is pruned to the latest 500 rows per pair.
6. **Main window.** Each pair shows a progress bar with bytes, files, speed and time left, the current file, and a Recent list with open/show-in-folder actions.
7. **Tray.** Left click opens a compact status window with the total and the last files. The total line also goes into the tooltip and the first menu item, since some tray hosts show the menu on left click and Wayland compositors place the window themselves. Tray updates are throttled.
8. **Tests.** Plan totals, meter arithmetic with fake stats replies, the space gate, activity recording and pruning.

## Limits

- Files are copied one at a time, so many small files show a low speed. Parallel transfers are a separate change.
- Remote objects without a size cannot be counted in byte totals.
