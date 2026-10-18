# Large files and folders that change constantly

Date: 2026-10-18. Implements two per-folder settings in the folder view:
a size limit for preserved copies and an optional synchronization interval.

This record exists because both settings change guarantees the rest of the
synchronization design makes, and because the reasons for the chosen
trade-offs are not visible from the code alone. It replaces nothing; the
content-aware reconciliation, guarded operations and recovery design in
`synchronization-plan.md` still applies, with the two exceptions listed under
"Residual limits".

## The problem

Neither rclone nor a cloud provider API can upload part of a file: a file is
uploaded whole, and replacing it consumes the whole file again. A 1.5 GB
database rewritten every five minutes therefore uploads about 18 GB an hour,
and each upload also becomes a preserved version, so recovery storage grows by
the same amount. Two things make it worse:

- A file being written offers no stable source. rclone refuses a source whose
  size or modification time changes while it is read (`can't copy - source
  file is being updated`), and Celestial re-checks the local tuple before and
  after every transfer. A file written every few minutes can fail every
  attempt, so it may never finish at all.
- Writes raise inotify events, which start a pass per burst. The pair spends
  all its time re-uploading one file.

## Setting 1: the largest file that keeps a preserved copy

`sync_dirs.recovery_max_bytes`, default `100_000_000` (100 MB), `0` keeps no
copies at all. Shown and edited in the folder view as "Keep a copy of files up
to ... MB".

Every replacement and deletion on both sides checks
`Store::keeps_version(size)` before preserving:

- Over the limit, the local file is removed (`unlink`) and the remote object
  is deleted (`operations/deletefile`) instead of being moved into the
  recovery folder.
- An upload that replaces a remote version over the limit is not journaled
  either: there is nothing to move back, so the journal entry would only
  describe work that cannot be undone.
- A remote item whose size the provider does not report is treated as small
  (preserved).
- The replay of an interrupted download applies the same limit, so a crash
  cannot leave one oversized version behind.

Everything else is unchanged: conflict copies, the conflict-copy cap, the
retention policy, and the restore path (`Restore` on a version that is already
preserved still works).

## Setting 2: synchronizing on an interval

`sync_dirs.sync_interval_secs`, default `0` (synchronize as changes are
detected), at most 30 days. Shown and edited in the folder view as "Only
synchronize this folder every ... minutes/hours/days", off by default.

A pair with an interval is "coarse" (`Pair::coarse`). Two things change.

**Scheduling.** `sync_pair` runs a pass only when the interval is due, a retry
is due, or the setting was just changed. Local events and the 60-second remote
check no longer start a pass; the watcher is still used, but only to decide
whether the last local observation can be reused. An unavailable root is
retried after 60 seconds rather than after a whole interval, so a drive that
mounts late is not ignored for hours. The schedule lives in the worker
(`Schedule::intervals`) and is not persisted: after a restart the folder is
synchronized once and then waits its interval again.

**Moving sources.** A coarse pass uploads the version of the file it observed
when the pass started:

- The "another program is still writing this file" quiet window is skipped:
  waiting for a file that is written every few minutes would postpone it
  forever.
- Every upload is made from a snapshot in Celestial's staging folder
  (the mechanism already used for providers without a hash). The snapshot is
  truncated to the observed size and carries the observed modification time,
  so what is uploaded is exactly what the pass decided on, and rclone's
  source-changed check cannot abort the transfer.
- A local hash that fails because the file changed is read as "changed"
  rather than "unknown"; `expect_local` before the upload and the
  transferred-tuple comparison after it are dropped, and the observation from
  the start of the pass is what is recorded.
- The baseline of a file that changed during its own upload records **no
  content fingerprint**. A fingerprint would claim the local file still holds
  what the remote has; without one, the next pass hashes the file and compares
  the two again. A folder that changes constantly therefore uploads once per
  interval, and a folder that has settled does not upload at all.

## Tests

- `existing_pairs_keep_a_copy_of_files_up_to_100_mb_and_sync_as_changes_arrive`
  (migration defaults).
- `a_file_bigger_than_the_limit_is_replaced_without_a_preserved_copy`,
  `a_file_bigger_than_the_limit_is_deleted_without_a_preserved_copy`,
  `a_limit_of_zero_keeps_no_preserved_copies_at_all`.
- `a_scheduled_pair_uploads_a_file_that_changes_during_the_pass` (the upload
  finishes, the remote holds the observed version, no fingerprint is recorded,
  the next pass uploads the newer version).
- `a_scheduled_pair_does_not_wait_for_a_file_still_being_written`.
- `an_interval_pair_ignores_local_changes_until_it_is_due` (drives `sync_pair`
  directly: not due means no pass, not even with a fresh edit).
- `the_interval_schedule_waits_and_can_be_forgotten`.

The existing tests about deferred writers, retries of a changed source and
preserved versions still pass unchanged: the coarse behavior is opt-in.

## Residual limits

- **A snapshot of a file being written is not a consistent copy.** `fs::copy`
  takes no lock. A file rewritten in place can be captured half old and half
  new. Pinning the size removes truncation from concurrent growth, but not
  torn content. This is the trade-off the setting exists for; a folder that
  needs consistent copies must not use an interval on a live file.
- **A coarse pair is coarse for everything in it.** Remote changes, new files
  and deletions in that folder are also seen only on the schedule. The
  interval is a per-folder setting, so a large live file should be its own
  folder.
- **An oversized version is gone once it is replaced.** That is the point of
  the limit, but it means the folder's recovery list is not a complete history
  of its big files, and provider version history (where it exists) becomes the
  only way back.
- **A limit change does not affect versions already preserved.** They stay
  until the retention policy removes them or they are deleted from the folder
  view.
- The snapshot costs one local copy of the file per upload. On a 1.5 GB file
  that is a local write of the same size as the upload, which is small next to
  the upload itself but not free.
