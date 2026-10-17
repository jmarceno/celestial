//! Persistent synchronization state: verified baselines, per-item states,
//! conflicts, the operation journal, recovery inventory and capabilities.
//!
//! Every function returns database errors; none of them turns a failed read
//! into an empty result.

use super::capabilities::Capabilities;
use super::observe::{Fingerprint, LocalMeta, Stamp};
use crate::entities::SyncDirsModel;
use crate::util;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, QueryResult,
    Statement, TransactionTrait, Value,
};
use std::cell::RefCell;
use std::collections::BTreeMap;

pub fn now() -> i64 {
    Stamp::now().secs
}

fn stmt(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Sqlite, sql, values)
}

pub fn exec<C: ConnectionTrait>(db: &C, sql: &str, values: Vec<Value>) -> Result<u64, DbErr> {
    util::await_future(db.execute(stmt(sql, values))).map(|result| result.rows_affected())
}

pub fn query<C: ConnectionTrait>(
    db: &C,
    sql: &str,
    values: Vec<Value>,
) -> Result<Vec<QueryResult>, DbErr> {
    util::await_future(db.query_all(stmt(sql, values)))
}

fn get<T: sea_orm::TryGetable>(row: &QueryResult, column: &str) -> Result<T, DbErr> {
    row.try_get::<T>("", column)
}

/// Inode and device numbers are unsigned; SQLite integers are signed.
fn to_db(value: u64) -> i64 {
    i64::from_ne_bytes(value.to_ne_bytes())
}

fn from_db(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

/// Open the database on a single connection. SQLite has one writer: with a
/// pool, the copy threads wait for each other in SQLite's busy handler,
/// which sleeps for growing intervals, instead of in the pool's queue. No
/// code path holds a transaction while it uses the connection outside it.
pub fn connect(path: &std::path::Path) -> Result<DatabaseConnection, String> {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .busy_timeout(std::time::Duration::from_secs(30));
    let pool = util::await_future(
        SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options),
    )
    .map_err(|err| err.to_string())?;
    Ok(sea_orm::SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

/// A write that records how an item ended, run inside the transaction
/// that settles the item.
pub type OutcomeWrite = Box<dyn FnOnce(&DatabaseTransaction) -> Result<(), DbErr> + Send>;

thread_local! {
    static OUTCOME: RefCell<Option<Vec<OutcomeWrite>>> = const { RefCell::new(None) };
}

/// Run `f`, keeping the outcome writes it makes instead of committing them.
/// The journal entry an operation writes before it changes anything is not
/// an outcome write: it is committed at once.
pub fn collecting_outcome<T>(f: impl FnOnce() -> T) -> (T, Vec<OutcomeWrite>) {
    OUTCOME.with(|outcome| *outcome.borrow_mut() = Some(Vec::new()));
    let result = f();
    let writes = OUTCOME.with(|outcome| outcome.borrow_mut().take().unwrap_or_default());
    (result, writes)
}

/// Record an outcome: kept when [`collecting_outcome`] is running on this
/// thread, otherwise committed now in its own transaction.
pub fn outcome(
    db: &DatabaseConnection,
    write: impl FnOnce(&DatabaseTransaction) -> Result<(), DbErr> + Send + 'static,
) -> Result<(), DbErr> {
    let write: OutcomeWrite = Box::new(write);
    let write = OUTCOME.with(|outcome| match outcome.borrow_mut().as_mut() {
        Some(kept) => {
            kept.push(write);
            None
        }
        None => Some(write),
    });
    match write {
        Some(write) => in_transaction(db, |tx| write(tx)),
        None => Ok(()),
    }
}

/// Run `f` in one transaction, committed when it succeeds.
pub fn in_transaction<T>(
    db: &DatabaseConnection,
    f: impl FnOnce(&DatabaseTransaction) -> Result<T, DbErr>,
) -> Result<T, DbErr> {
    let tx = util::await_future(db.begin())?;
    let value = f(&tx)?;
    util::await_future(tx.commit())?;
    Ok(value)
}

/// Use write-ahead logging: a commit then syncs one file once instead of
/// several, which is most of the cost of synchronizing many small files.
/// Commits stay durable (`synchronous` is left at its default). The mode is
/// kept in the database file; a filesystem without the shared memory WAL
/// needs keeps the rollback journal.
pub fn use_wal<C: ConnectionTrait>(db: &C) {
    match query(db, "PRAGMA journal_mode=WAL", vec![]) {
        Ok(rows) => {
            let mode = rows
                .first()
                .and_then(|row| get::<String>(row, "journal_mode").ok())
                .unwrap_or_default();
            if mode != "wal" {
                tracing::warn!(mode, "the database could not use write-ahead logging");
            }
        }
        Err(err) => tracing::warn!("could not enable write-ahead logging: {err}"),
    }
}

/// Copy the database to `to`, including commits still in the write-ahead
/// log, so the copy is complete on its own.
pub fn copy_database<C: ConnectionTrait>(
    db: &C,
    from: &std::path::Path,
    to: &std::path::Path,
) -> Result<(), String> {
    query(db, "PRAGMA wal_checkpoint(TRUNCATE)", vec![]).map_err(|err| err.to_string())?;
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// The pair's stable identity, assigning one if it has none yet.
pub fn pair_uuid<C: ConnectionTrait>(db: &C, sync_dir_id: i32) -> Result<String, DbErr> {
    exec(
        db,
        "UPDATE sync_dirs SET pair_uuid = lower(hex(randomblob(16))) WHERE id = ? AND pair_uuid IS NULL",
        vec![sync_dir_id.into()],
    )?;
    let rows = query(
        db,
        "SELECT pair_uuid FROM sync_dirs WHERE id = ?",
        vec![sync_dir_id.into()],
    )?;
    let row = rows
        .first()
        .ok_or_else(|| DbErr::RecordNotFound("sync pair".into()))?;
    get::<String>(row, "pair_uuid")
}

/// Advance the pair generation; every committed operation gets a new one.
pub fn next_generation<C: ConnectionTrait>(db: &C, sync_dir_id: i32) -> Result<i64, DbErr> {
    exec(
        db,
        "UPDATE sync_dirs SET generation = generation + 1 WHERE id = ?",
        vec![sync_dir_id.into()],
    )?;
    let rows = query(
        db,
        "SELECT generation FROM sync_dirs WHERE id = ?",
        vec![sync_dir_id.into()],
    )?;
    get::<i64>(
        rows.first()
            .ok_or_else(|| DbErr::RecordNotFound("sync pair".into()))?,
        "generation",
    )
}

/// Pairs that wait for the person to start them.
pub fn paused_pairs<C: ConnectionTrait>(db: &C) -> Result<std::collections::HashSet<i32>, DbErr> {
    query(db, "SELECT id FROM sync_dirs WHERE paused != 0", vec![])?
        .iter()
        .map(|row| get::<i32>(row, "id"))
        .collect()
}

pub fn set_paused<C: ConnectionTrait>(db: &C, sync_dir_id: i32, paused: bool) -> Result<(), DbErr> {
    exec(
        db,
        "UPDATE sync_dirs SET paused = ? WHERE id = ?",
        vec![paused.into(), sync_dir_id.into()],
    )
    .map(|_| ())
}

/// How many days the pair keeps preserved versions; 0 keeps them for good.
pub fn keep_versions_days<C: ConnectionTrait>(db: &C, sync_dir_id: i32) -> Result<i64, DbErr> {
    query(
        db,
        "SELECT keep_versions_days FROM sync_dirs WHERE id = ?",
        vec![sync_dir_id.into()],
    )?
    .first()
    .map_or(Ok(30), |row| get(row, "keep_versions_days"))
}

pub fn set_keep_versions_days<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    days: i64,
) -> Result<(), DbErr> {
    exec(
        db,
        "UPDATE sync_dirs SET keep_versions_days = ? WHERE id = ?",
        vec![days.max(0).into(), sync_dir_id.into()],
    )
    .map(|_| ())
}

/// The recorded name and (device, inode) of the pair's internal folder.
/// A folder name and its (device, inode).
pub type FolderIdentity = (String, (u64, u64));

pub fn internal_dir<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
) -> Result<Option<FolderIdentity>, DbErr> {
    let rows = query(
        db,
        "SELECT internal_name, internal_dev, internal_ino FROM sync_dirs WHERE id = ?",
        vec![sync_dir_id.into()],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let name: Option<String> = get(row, "internal_name")?;
    let dev: Option<i64> = get(row, "internal_dev")?;
    let ino: Option<i64> = get(row, "internal_ino")?;
    Ok(match (name, dev, ino) {
        (Some(name), Some(dev), Some(ino)) => Some((name, (from_db(dev), from_db(ino)))),
        _ => None,
    })
}

pub fn set_internal_dir<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    name: &str,
    identity: (u64, u64),
) -> Result<(), DbErr> {
    exec(
        db,
        "UPDATE sync_dirs SET internal_name = ?, internal_dev = ?, internal_ino = ? WHERE id = ?",
        vec![
            name.into(),
            to_db(identity.0).into(),
            to_db(identity.1).into(),
            sync_dir_id.into(),
        ],
    )
    .map(|_| ())
}

// ---------------------------------------------------------------- baselines

pub const SYNCED: &str = "synced";

/// The last common contents of an item, and what each side looked like then.
#[derive(Clone, Debug, PartialEq)]
pub struct Baseline {
    pub id: i64,
    pub state: String,
    pub is_dir: Option<bool>,
    pub size: Option<i64>,
    pub fingerprint: Option<Fingerprint>,
    /// The local tuple, when recorded by a content-aware version.
    pub local: Option<LocalMeta>,
    /// Whole-second local time from any version, kept for legacy rows.
    pub local_secs: i64,
    pub remote_size: Option<i64>,
    pub remote_mtime: Stamp,
    pub remote_hash: Option<String>,
    pub remote_id: Option<String>,
    pub generation: i64,
}

impl Baseline {
    /// Only rows written after verifying both sides are a common baseline.
    pub fn verified(&self) -> bool {
        self.state == SYNCED
    }
}

/// What to record after both sides were verified to hold the same contents.
#[derive(Clone, Debug)]
pub struct BaselineWrite {
    pub is_dir: bool,
    pub size: i64,
    pub fingerprint: Option<Fingerprint>,
    pub local: LocalMeta,
    /// `false` for a file modified within the timestamp granularity of its
    /// observation: its tuple is not trusted, so the next pass compares its
    /// contents before relying on it.
    pub trust_local: bool,
    pub remote_size: i64,
    pub remote_mtime: Stamp,
    pub remote_hash: Option<String>,
    pub remote_id: Option<String>,
    pub generation: i64,
}

/// Absolute local and provider-relative remote paths of an item, as stored.
pub fn item_paths(sync_dir: &SyncDirsModel, rel: &str) -> (String, String) {
    let local = format!("{}/{rel}", sync_dir.local_path.trim_end_matches('/'));
    let remote = if sync_dir.remote_path.is_empty() {
        rel.to_owned()
    } else {
        format!("{}/{rel}", sync_dir.remote_path)
    };
    (local, remote)
}

/// The pair's history by relative path, in one read.
pub fn load_baselines<C: ConnectionTrait>(
    db: &C,
    sync_dir: &SyncDirsModel,
) -> Result<BTreeMap<String, Baseline>, DbErr> {
    let rows = query(
        db,
        "SELECT * FROM sync_items WHERE sync_dir_id = ?",
        vec![sync_dir.id.into()],
    )?;
    let root = format!("{}/", sync_dir.local_path.trim_end_matches('/'));
    let mut out = BTreeMap::new();
    for row in rows {
        let local_path: String = get(&row, "local_path")?;
        let remote_path: String = get(&row, "remote_path")?;
        let Some(rel) = local_path.strip_prefix(&root) else {
            continue;
        };
        if item_paths(sync_dir, rel).1 != remote_path {
            continue;
        }
        let hash_type: Option<String> = get(&row, "hash_type")?;
        let hash: Option<String> = get(&row, "hash")?;
        let inode: Option<i64> = get(&row, "local_inode")?;
        let is_dir: Option<i64> = get(&row, "is_dir")?;
        let local_secs: i64 = get(&row, "last_local_timestamp")?;
        let local = match (inode, is_dir) {
            (Some(inode), Some(is_dir)) => Some(LocalMeta {
                is_dir: is_dir != 0,
                size: get::<Option<i64>>(&row, "size")?
                    .and_then(|size| u64::try_from(size).ok())
                    .unwrap_or(0),
                mtime: Stamp::new(
                    local_secs,
                    get::<Option<i64>>(&row, "local_mtime_nsec")?.unwrap_or(0),
                ),
                ctime: Stamp::new(
                    get::<Option<i64>>(&row, "local_ctime")?.unwrap_or(0),
                    get::<Option<i64>>(&row, "local_ctime_nsec")?.unwrap_or(0),
                ),
                inode: from_db(inode),
                device: from_db(get::<Option<i64>>(&row, "local_device")?.unwrap_or(0)),
            }),
            _ => None,
        };
        out.insert(
            rel.to_owned(),
            Baseline {
                id: get(&row, "id")?,
                state: get(&row, "state")?,
                is_dir: is_dir.map(|value| value != 0),
                size: get(&row, "size")?,
                fingerprint: hash_type
                    .zip(hash)
                    .map(|(algo, value)| Fingerprint { algo, value }),
                local,
                local_secs,
                remote_size: get(&row, "remote_size")?,
                remote_mtime: Stamp::new(
                    get(&row, "last_remote_timestamp")?,
                    get::<Option<i64>>(&row, "remote_mtime_nsec")?.unwrap_or(0),
                ),
                remote_hash: get(&row, "remote_hash")?,
                remote_id: get(&row, "remote_id")?,
                generation: get(&row, "generation")?,
            },
        );
    }
    Ok(out)
}

/// Record a verified common baseline, replacing any earlier row.
pub fn save_baseline<C: ConnectionTrait>(
    db: &C,
    sync_dir: &SyncDirsModel,
    rel: &str,
    write: &BaselineWrite,
) -> Result<(), DbErr> {
    let (local_path, remote_path) = item_paths(sync_dir, rel);
    let (hash_type, hash) = match &write.fingerprint {
        Some(fp) => (Some(fp.algo.clone()), Some(fp.value.clone())),
        None => (None, None),
    };
    exec(
        db,
        "INSERT INTO sync_items (sync_dir_id, local_path, remote_path, last_local_timestamp,
            last_remote_timestamp, state, is_dir, size, hash_type, hash, local_mtime_nsec,
            local_ctime, local_ctime_nsec, local_inode, local_device, remote_size,
            remote_mtime_nsec, remote_hash, remote_id, generation, verified_at)
         VALUES (?, ?, ?, ?, ?, 'synced', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (sync_dir_id, local_path, remote_path) DO UPDATE SET
            last_local_timestamp = excluded.last_local_timestamp,
            last_remote_timestamp = excluded.last_remote_timestamp,
            state = 'synced', is_dir = excluded.is_dir, size = excluded.size,
            hash_type = excluded.hash_type, hash = excluded.hash,
            local_mtime_nsec = excluded.local_mtime_nsec, local_ctime = excluded.local_ctime,
            local_ctime_nsec = excluded.local_ctime_nsec, local_inode = excluded.local_inode,
            local_device = excluded.local_device, remote_size = excluded.remote_size,
            remote_mtime_nsec = excluded.remote_mtime_nsec, remote_hash = excluded.remote_hash,
            remote_id = excluded.remote_id, generation = excluded.generation,
            verified_at = excluded.verified_at",
        vec![
            sync_dir.id.into(),
            local_path.into(),
            remote_path.into(),
            write.local.mtime.secs.into(),
            write.remote_mtime.secs.into(),
            i64::from(write.is_dir).into(),
            write.size.into(),
            hash_type.into(),
            hash.into(),
            i64::from(write.local.mtime.nanos).into(),
            write.local.ctime.secs.into(),
            i64::from(write.local.ctime.nanos).into(),
            write.trust_local.then(|| to_db(write.local.inode)).into(),
            to_db(write.local.device).into(),
            write.remote_size.into(),
            i64::from(write.remote_mtime.nanos).into(),
            write.remote_hash.clone().into(),
            write.remote_id.clone().into(),
            write.generation.into(),
            now().into(),
        ],
    )
    .map(|_| ())
}

/// Retire an item's history after both sides were confirmed absent, or after
/// a deletion was propagated and verified.
pub fn delete_baseline<C: ConnectionTrait>(db: &C, id: i64) -> Result<(), DbErr> {
    exec(db, "DELETE FROM sync_items WHERE id = ?", vec![id.into()]).map(|_| ())
}

// -------------------------------------------------------------- item states

pub const RETRYING: &str = "retrying";
pub const BLOCKED: &str = "blocked";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemState {
    pub rel: String,
    pub state: String,
    pub attempts: i64,
    pub next_attempt: Option<i64>,
    pub reason: String,
    /// A limitation warning the person dismissed; it is not reported again.
    pub dismissed: bool,
}

/// First retry after one minute, doubling to at most one hour.
pub fn backoff_secs(attempts: i64) -> i64 {
    let exponent = u32::try_from(attempts.clamp(1, 7) - 1).unwrap_or(0);
    (60 * 2i64.pow(exponent)).min(3600)
}

pub fn load_item_states<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
) -> Result<BTreeMap<String, ItemState>, DbErr> {
    query(
        db,
        "SELECT rel_path, state, attempts, next_attempt, reason, dismissed FROM item_states WHERE sync_dir_id = ?",
        vec![sync_dir_id.into()],
    )?
    .iter()
    .map(|row| {
        let rel: String = get(row, "rel_path")?;
        let dismissed: i64 = get(row, "dismissed")?;
        Ok((
            rel.clone(),
            ItemState {
                rel,
                state: get(row, "state")?,
                attempts: get(row, "attempts")?,
                next_attempt: get(row, "next_attempt")?,
                reason: get(row, "reason")?,
                dismissed: dismissed != 0,
            },
        ))
    })
    .collect()
}

/// Mark an item for an automatic retry with exponential backoff.
pub fn set_retrying<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    rel: &str,
    reason: &str,
) -> Result<ItemState, DbErr> {
    let attempts = query(
        db,
        "SELECT attempts FROM item_states WHERE sync_dir_id = ? AND rel_path = ? AND state = 'retrying'",
        vec![sync_dir_id.into(), rel.into()],
    )?
    .first()
    .map(|row| get::<i64>(row, "attempts"))
    .transpose()?
    .unwrap_or(0)
        + 1;
    let next = now() + backoff_secs(attempts);
    set_state(db, sync_dir_id, rel, RETRYING, attempts, Some(next), reason)?;
    Ok(ItemState {
        rel: rel.to_owned(),
        state: RETRYING.into(),
        attempts,
        next_attempt: Some(next),
        reason: reason.to_owned(),
        dismissed: false,
    })
}

/// Hold an item until `next_attempt` without counting a failed attempt:
/// another program is still writing it.
pub fn set_waiting<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    rel: &str,
    reason: &str,
    next_attempt: i64,
) -> Result<(), DbErr> {
    set_state(
        db,
        sync_dir_id,
        rel,
        RETRYING,
        0,
        Some(next_attempt),
        reason,
    )
}

/// Block an item until the person changes it (renames a collision, etc.).
pub fn set_blocked<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    rel: &str,
    reason: &str,
) -> Result<(), DbErr> {
    set_state(db, sync_dir_id, rel, BLOCKED, 0, None, reason)
}

fn set_state<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    rel: &str,
    state: &str,
    attempts: i64,
    next_attempt: Option<i64>,
    reason: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        "INSERT INTO item_states (sync_dir_id, rel_path, state, attempts, next_attempt, reason, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (sync_dir_id, rel_path) DO UPDATE SET state = excluded.state,
            attempts = excluded.attempts, next_attempt = excluded.next_attempt,
            reason = excluded.reason, updated_at = excluded.updated_at",
        vec![
            sync_dir_id.into(),
            rel.into(),
            state.into(),
            attempts.into(),
            next_attempt.into(),
            reason.into(),
            now().into(),
        ],
    )
    .map(|_| ())
}

pub fn clear_item_state<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    rel: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        "DELETE FROM item_states WHERE sync_dir_id = ? AND rel_path = ?",
        vec![sync_dir_id.into(), rel.into()],
    )
    .map(|_| ())
}

/// Remember that the person dismissed a limitation warning. The item stays
/// blocked; the flag keeps the next pass from reporting it again.
pub fn dismiss_item<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    rel: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        "UPDATE item_states SET dismissed = 1, updated_at = ? WHERE sync_dir_id = ? AND rel_path = ?",
        vec![now().into(), sync_dir_id.into(), rel.into()],
    )
    .map(|_| ())
}

/// Retry an item on the next pass (after dismissal or restart).
pub fn retry_now<C: ConnectionTrait>(db: &C, sync_dir_id: i32, rel: &str) -> Result<(), DbErr> {
    exec(
        db,
        "UPDATE item_states SET next_attempt = ? WHERE sync_dir_id = ? AND rel_path = ?",
        vec![now().into(), sync_dir_id.into(), rel.into()],
    )
    .map(|_| ())
}

// ---------------------------------------------------------------- conflicts

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRecord {
    pub id: i64,
    pub rel: String,
    pub kind: String,
    /// JSON describing both versions and where each one now lives.
    pub detail: String,
    pub conflict_copy: Option<String>,
    pub op_id: String,
    pub created_at: i64,
}

#[allow(clippy::too_many_arguments)]
pub fn insert_conflict<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    pair_uuid: &str,
    rel: &str,
    kind: &str,
    detail: &str,
    conflict_copy: Option<&str>,
    op_id: &str,
) -> Result<(), DbErr> {
    // The operation ID makes recording idempotent across retries.
    exec(
        db,
        "INSERT INTO conflicts (sync_dir_id, pair_uuid, rel_path, kind, detail, conflict_copy, op_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (op_id) DO NOTHING",
        vec![
            sync_dir_id.into(),
            pair_uuid.into(),
            rel.into(),
            kind.into(),
            detail.into(),
            conflict_copy.map(str::to_owned).into(),
            op_id.into(),
            now().into(),
        ],
    )
    .map(|_| ())
}

pub fn open_conflicts<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
) -> Result<Vec<ConflictRecord>, DbErr> {
    query(
        db,
        "SELECT * FROM conflicts WHERE sync_dir_id = ? AND acknowledged = 0 ORDER BY id",
        vec![sync_dir_id.into()],
    )?
    .iter()
    .map(|row| {
        Ok(ConflictRecord {
            id: get(row, "id")?,
            rel: get(row, "rel_path")?,
            kind: get(row, "kind")?,
            detail: get(row, "detail")?,
            conflict_copy: get(row, "conflict_copy")?,
            op_id: get(row, "op_id")?,
            created_at: get(row, "created_at")?,
        })
    })
    .collect()
}

pub fn acknowledge_conflict<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    id: i64,
) -> Result<bool, DbErr> {
    exec(
        db,
        "UPDATE conflicts SET acknowledged = 1 WHERE sync_dir_id = ? AND id = ?",
        vec![sync_dir_id.into(), id.into()],
    )
    .map(|rows| rows > 0)
}

// --------------------------------------------------------------- operations

/// Journal payloads carry a format version; unknown versions are refused.
pub const JOURNAL_VERSION: i64 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub sync_dir_id: i32,
    pub pair_uuid: String,
    pub rel: String,
    pub action: String,
    pub state: String,
    pub payload: serde_json::Value,
}

pub fn insert_operation<C: ConnectionTrait>(db: &C, op: &Operation) -> Result<(), DbErr> {
    let mut payload = op.payload.clone();
    payload["v"] = JOURNAL_VERSION.into();
    exec(
        db,
        "INSERT INTO operations (id, sync_dir_id, pair_uuid, rel_path, action, state, payload, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            op.id.clone().into(),
            op.sync_dir_id.into(),
            op.pair_uuid.clone().into(),
            op.rel.clone().into(),
            op.action.clone().into(),
            op.state.clone().into(),
            payload.to_string().into(),
            now().into(),
            now().into(),
        ],
    )
    .map(|_| ())
}

pub fn update_operation<C: ConnectionTrait>(
    db: &C,
    id: &str,
    state: &str,
    payload: Option<&serde_json::Value>,
    error: Option<&str>,
) -> Result<(), DbErr> {
    match payload {
        Some(payload) => {
            let mut payload = payload.clone();
            payload["v"] = JOURNAL_VERSION.into();
            exec(
                db,
                "UPDATE operations SET state = ?, payload = ?, error = ?, updated_at = ? WHERE id = ?",
                vec![
                    state.into(),
                    payload.to_string().into(),
                    error.map(str::to_owned).into(),
                    now().into(),
                    id.into(),
                ],
            )
        }
        None => exec(
            db,
            "UPDATE operations SET state = ?, error = ?, updated_at = ? WHERE id = ?",
            vec![
                state.into(),
                error.map(str::to_owned).into(),
                now().into(),
                id.into(),
            ],
        ),
    }
    .map(|_| ())
}

/// Operations that did not reach a final state, oldest first. A payload in an
/// unknown format is an error rather than something to guess at.
pub fn incomplete_operations<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: Option<i32>,
) -> Result<Vec<Operation>, DbErr> {
    let rows = match sync_dir_id {
        Some(id) => query(
            db,
            "SELECT * FROM operations WHERE state NOT IN ('complete', 'failed') AND sync_dir_id = ? ORDER BY created_at, rowid",
            vec![id.into()],
        )?,
        None => query(
            db,
            "SELECT * FROM operations WHERE state NOT IN ('complete', 'failed') ORDER BY created_at, rowid",
            vec![],
        )?,
    };
    rows.iter()
        .map(|row| {
            let text: String = get(row, "payload")?;
            let payload: serde_json::Value = serde_json::from_str(&text)
                .map_err(|err| DbErr::Custom(format!("Unreadable journal entry: {err}")))?;
            if payload.get("v").and_then(serde_json::Value::as_i64) != Some(JOURNAL_VERSION) {
                return Err(DbErr::Custom(
                    "The operation journal was written by a different version of Celestial".into(),
                ));
            }
            Ok(Operation {
                id: get(row, "id")?,
                sync_dir_id: get(row, "sync_dir_id")?,
                pair_uuid: get(row, "pair_uuid")?,
                rel: get(row, "rel_path")?,
                action: get(row, "action")?,
                state: get(row, "state")?,
                payload,
            })
        })
        .collect()
}

/// Forget finished journal entries older than `age` seconds.
pub fn prune_operations<C: ConnectionTrait>(db: &C, age: i64) -> Result<(), DbErr> {
    exec(
        db,
        "DELETE FROM operations WHERE state IN ('complete', 'failed') AND updated_at < ?",
        vec![(now() - age).into()],
    )
    .map(|_| ())
}

// ----------------------------------------------------------------- activity

/// One synchronized file, for the list of recent changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activity {
    pub rel: String,
    /// `uploaded`, `downloaded`, `deleted_local`, `deleted_remote` or
    /// `conflict`.
    pub action: String,
    pub size: i64,
    pub finished_at: i64,
}

/// Recent changes kept per pair.
pub const ACTIVITY_KEPT: i64 = 500;

pub fn insert_activity<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    activity: &Activity,
) -> Result<(), DbErr> {
    exec(
        db,
        "INSERT INTO sync_activity (sync_dir_id, rel, action, size, finished_at) VALUES (?, ?, ?, ?, ?)",
        vec![
            sync_dir_id.into(),
            activity.rel.clone().into(),
            activity.action.clone().into(),
            activity.size.into(),
            activity.finished_at.into(),
        ],
    )
    .map(|_| ())
}

/// The latest changes of a pair, newest first.
pub fn recent_activity<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    limit: i64,
) -> Result<Vec<Activity>, DbErr> {
    query(
        db,
        "SELECT rel, action, size, finished_at FROM sync_activity WHERE sync_dir_id = ? ORDER BY id DESC LIMIT ?",
        vec![sync_dir_id.into(), limit.into()],
    )?
    .iter()
    .map(|row| {
        Ok(Activity {
            rel: get(row, "rel")?,
            action: get(row, "action")?,
            size: get(row, "size")?,
            finished_at: get(row, "finished_at")?,
        })
    })
    .collect()
}

/// Keep only the latest `keep` changes of a pair.
pub fn prune_activity<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    keep: i64,
) -> Result<(), DbErr> {
    exec(
        db,
        "DELETE FROM sync_activity WHERE sync_dir_id = ? AND id NOT IN
         (SELECT id FROM sync_activity WHERE sync_dir_id = ? ORDER BY id DESC LIMIT ?)",
        vec![sync_dir_id.into(), sync_dir_id.into(), keep.into()],
    )
    .map(|_| ())
}

// ----------------------------------------------------------------- recovery

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryItem {
    pub id: i64,
    pub pair_uuid: String,
    pub remote_name: String,
    pub local_root: String,
    pub remote_root: String,
    pub rel: String,
    /// `local` or `remote`: where the preserved copy is stored.
    pub side: String,
    pub stored_path: String,
    pub reason: String,
    pub op_id: String,
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    pub created_at: i64,
}

pub fn insert_recovery<C: ConnectionTrait>(db: &C, item: &RecoveryItem) -> Result<(), DbErr> {
    exec(
        db,
        "INSERT INTO recovery_items (pair_uuid, remote_name, local_root, remote_root, rel_path, side,
            stored_path, reason, op_id, size, mtime, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            item.pair_uuid.clone().into(),
            item.remote_name.clone().into(),
            item.local_root.clone().into(),
            item.remote_root.clone().into(),
            item.rel.clone().into(),
            item.side.clone().into(),
            item.stored_path.clone().into(),
            item.reason.clone().into(),
            item.op_id.clone().into(),
            item.size.into(),
            item.mtime.into(),
            item.created_at.into(),
        ],
    )
    .map(|_| ())
}

/// Preserved versions still available for a pair, newest first.
pub fn list_recovery<C: ConnectionTrait>(
    db: &C,
    pair_uuid: &str,
) -> Result<Vec<RecoveryItem>, DbErr> {
    query(
        db,
        "SELECT * FROM recovery_items WHERE pair_uuid = ? AND pruned_at IS NULL ORDER BY created_at DESC, id DESC",
        vec![pair_uuid.into()],
    )?
    .iter()
    .map(|row| {
        Ok(RecoveryItem {
            id: get(row, "id")?,
            pair_uuid: get(row, "pair_uuid")?,
            remote_name: get(row, "remote_name")?,
            local_root: get(row, "local_root")?,
            remote_root: get(row, "remote_root")?,
            rel: get(row, "rel_path")?,
            side: get(row, "side")?,
            stored_path: get(row, "stored_path")?,
            reason: get(row, "reason")?,
            op_id: get(row, "op_id")?,
            size: get(row, "size")?,
            mtime: get(row, "mtime")?,
            created_at: get(row, "created_at")?,
        })
    })
    .collect()
}

pub fn mark_recovery<C: ConnectionTrait>(db: &C, id: i64, column: &str) -> Result<(), DbErr> {
    let sql = match column {
        "restored_at" => "UPDATE recovery_items SET restored_at = ? WHERE id = ?",
        _ => "UPDATE recovery_items SET pruned_at = ? WHERE id = ?",
    };
    exec(db, sql, vec![now().into(), id.into()]).map(|_| ())
}

/// Operations whose preserved versions must not be pruned: unresolved
/// conflicts and operations that did not finish.
pub fn protected_operations<C: ConnectionTrait>(
    db: &C,
    pair_uuid: &str,
) -> Result<std::collections::HashSet<String>, DbErr> {
    query(
        db,
        "SELECT op_id AS id FROM conflicts WHERE pair_uuid = ? AND acknowledged = 0
         UNION SELECT id FROM operations WHERE pair_uuid = ? AND state NOT IN ('complete', 'failed')",
        vec![pair_uuid.into(), pair_uuid.into()],
    )?
    .iter()
    .map(|row| get::<String>(row, "id"))
    .collect()
}

/// Whether a preserved version stored at `stored_path` by operation `op`
/// is recorded.
pub fn recovery_for_op<C: ConnectionTrait>(
    db: &C,
    op: &str,
    stored_path: &str,
) -> Result<bool, DbErr> {
    Ok(!query(
        db,
        "SELECT id FROM recovery_items WHERE op_id = ? AND stored_path = ?",
        vec![op.into(), stored_path.into()],
    )?
    .is_empty())
}

/// Where operation `op` preserved the `side` version of `rel`, if it did.
pub fn recovery_of_op<C: ConnectionTrait>(
    db: &C,
    op: &str,
    side: &str,
    rel: &str,
) -> Result<Option<String>, DbErr> {
    query(
        db,
        "SELECT stored_path FROM recovery_items WHERE op_id = ? AND side = ? AND rel_path = ? AND pruned_at IS NULL ORDER BY id DESC",
        vec![op.into(), side.into(), rel.into()],
    )?
    .first()
    .map(|row| get::<String>(row, "stored_path"))
    .transpose()
}

/// A preserved version was moved back where it came from: it is no longer
/// stored in recovery, so it is not offered for restoring.
pub fn forget_recovery<C: ConnectionTrait>(
    db: &C,
    op: &str,
    stored_path: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        "UPDATE recovery_items SET pruned_at = ? WHERE op_id = ? AND stored_path = ?",
        vec![now().into(), op.into(), stored_path.into()],
    )
    .map(|_| ())
}

pub fn conflict_for_op<C: ConnectionTrait>(db: &C, op: &str) -> Result<bool, DbErr> {
    Ok(!query(
        db,
        "SELECT id FROM conflicts WHERE op_id = ?",
        vec![op.into()],
    )?
    .is_empty())
}

pub fn recovery_by_id<C: ConnectionTrait>(
    db: &C,
    pair_uuid: &str,
    id: i64,
) -> Result<Option<RecoveryItem>, DbErr> {
    Ok(list_recovery(db, pair_uuid)?
        .into_iter()
        .find(|item| item.id == id))
}

pub fn conflict_by_id<C: ConnectionTrait>(
    db: &C,
    sync_dir_id: i32,
    id: i64,
) -> Result<Option<ConflictRecord>, DbErr> {
    Ok(open_conflicts(db, sync_dir_id)?
        .into_iter()
        .find(|record| record.id == id))
}

// ------------------------------------------------------------- capabilities

pub fn save_capabilities<C: ConnectionTrait>(
    db: &C,
    remote_name: &str,
    caps: &Capabilities,
    rclone_version: &str,
) -> Result<(), DbErr> {
    exec(
        db,
        "INSERT INTO remote_capabilities (remote_name, hashes, precision_ns, case_insensitive,
            duplicate_files, partial_uploads, server_move, server_copy, empty_dirs, set_modtime,
            list_r, trash, rclone_version, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (remote_name) DO UPDATE SET hashes = excluded.hashes,
            precision_ns = excluded.precision_ns, case_insensitive = excluded.case_insensitive,
            duplicate_files = excluded.duplicate_files, partial_uploads = excluded.partial_uploads,
            server_move = excluded.server_move, server_copy = excluded.server_copy,
            empty_dirs = excluded.empty_dirs, set_modtime = excluded.set_modtime,
            list_r = excluded.list_r, trash = excluded.trash,
            rclone_version = excluded.rclone_version, updated_at = excluded.updated_at",
        vec![
            remote_name.into(),
            serde_json::to_string(&caps.hashes)
                .unwrap_or_else(|_| "[]".into())
                .into(),
            caps.precision_ns.into(),
            caps.case_insensitive.into(),
            caps.duplicate_files.into(),
            caps.partial_uploads.into(),
            caps.server_move.into(),
            caps.server_copy.into(),
            caps.empty_dirs.into(),
            caps.set_modtime.into(),
            caps.list_r.into(),
            caps.trash.clone().into(),
            rclone_version.into(),
            now().into(),
        ],
    )
    .map(|_| ())
}

pub fn load_capabilities<C: ConnectionTrait>(
    db: &C,
    remote_name: &str,
) -> Result<Option<Capabilities>, DbErr> {
    let rows = query(
        db,
        "SELECT * FROM remote_capabilities WHERE remote_name = ?",
        vec![remote_name.into()],
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let hashes: String = get(row, "hashes")?;
    Ok(Some(Capabilities {
        hashes: serde_json::from_str(&hashes).map_err(|err| DbErr::Custom(err.to_string()))?,
        precision_ns: get(row, "precision_ns")?,
        case_insensitive: get(row, "case_insensitive")?,
        duplicate_files: get(row, "duplicate_files")?,
        partial_uploads: get(row, "partial_uploads")?,
        server_move: get(row, "server_move")?,
        server_copy: get(row, "server_copy")?,
        empty_dirs: get(row, "empty_dirs")?,
        set_modtime: get(row, "set_modtime")?,
        list_r: get(row, "list_r")?,
        trash: get(row, "trash")?,
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::migrations::{Migrator, MigratorTrait};
    use sea_orm::{Database, DatabaseConnection};

    pub fn memory_db() -> DatabaseConnection {
        let db = util::await_future(Database::connect("sqlite::memory:")).unwrap();
        util::await_future(Migrator::up(&db, None)).unwrap();
        db
    }

    fn pair(db: &DatabaseConnection, local: &str) -> SyncDirsModel {
        exec(db, "INSERT INTO remotes (name) VALUES ('r')", vec![]).ok();
        exec(
            db,
            "INSERT INTO sync_dirs (remote_id, local_path, remote_path) VALUES (1, ?, 'root')",
            vec![local.into()],
        )
        .unwrap();
        let id = query(db, "SELECT max(id) AS id FROM sync_dirs", vec![]).unwrap()[0]
            .try_get::<i32>("", "id")
            .unwrap();
        SyncDirsModel {
            id,
            remote_id: 1,
            local_path: local.into(),
            remote_path: "root".into(),
            pair_uuid: None,
            generation: 0,
        }
    }

    fn meta() -> LocalMeta {
        LocalMeta {
            is_dir: false,
            size: 3,
            mtime: Stamp::new(-100, 5),
            ctime: Stamp::new(4_000_000_000, 7),
            inode: u64::MAX - 1,
            device: 9,
        }
    }

    #[test]
    fn baselines_round_trip_with_signed_times_and_large_inodes() {
        let db = memory_db();
        let dir = pair(&db, "/l");
        let write = BaselineWrite {
            is_dir: false,
            size: 3,
            fingerprint: Some(Fingerprint {
                algo: "md5".into(),
                value: "abc".into(),
            }),
            local: meta(),
            trust_local: true,
            remote_size: 3,
            remote_mtime: Stamp::new(-100, 0),
            remote_hash: Some("abc".into()),
            remote_id: Some("id".into()),
            generation: 2,
        };
        save_baseline(&db, &dir, "a/b", &write).unwrap();
        save_baseline(&db, &dir, "a/b", &write).unwrap();
        let rows = load_baselines(&db, &dir).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows["a/b"];
        assert!(row.verified());
        assert_eq!(row.local, Some(meta()));
        assert_eq!(row.remote_mtime, Stamp::new(-100, 0));
        assert_eq!(row.fingerprint.as_ref().unwrap().value, "abc");
    }

    #[test]
    fn pairs_with_identical_paths_keep_separate_state() {
        let db = memory_db();
        let first = pair(&db, "/l");
        let second = pair(&db, "/l");
        set_retrying(&db, first.id, "x", "offline").unwrap();
        assert!(load_item_states(&db, second.id).unwrap().is_empty());
        assert_ne!(
            pair_uuid(&db, first.id).unwrap(),
            pair_uuid(&db, second.id).unwrap()
        );
    }

    #[test]
    fn retries_back_off_from_one_minute_to_one_hour() {
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(30), 3600);
        let db = memory_db();
        let dir = pair(&db, "/l");
        assert_eq!(set_retrying(&db, dir.id, "x", "a").unwrap().attempts, 1);
        assert_eq!(set_retrying(&db, dir.id, "x", "b").unwrap().attempts, 2);
    }

    #[test]
    fn a_copy_of_a_wal_database_holds_every_commit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("celestial.db");
        std::fs::File::create(&path).unwrap();
        let url = |path: &std::path::Path| format!("sqlite://{}", path.display());
        let db = util::await_future(Database::connect(url(&path))).unwrap();
        use_wal(&db);
        let mode = query(&db, "PRAGMA journal_mode", vec![]).unwrap();
        assert_eq!(get::<String>(&mode[0], "journal_mode").unwrap(), "wal");
        util::await_future(Migrator::up(&db, None)).unwrap();
        exec(&db, "INSERT INTO remotes (name) VALUES ('kept')", vec![]).unwrap();
        let copy = temp.path().join("copy.db");
        copy_database(&db, &path, &copy).unwrap();
        let copied = util::await_future(Database::connect(url(&copy))).unwrap();
        let rows = query(&copied, "SELECT name FROM remotes", vec![]).unwrap();
        assert_eq!(get::<String>(&rows[0], "name").unwrap(), "kept");
    }

    #[test]
    fn recent_activity_is_newest_first_per_pair_and_pruned() {
        let db = memory_db();
        let first = pair(&db, "/a");
        let second = pair(&db, "/b");
        for index in 0..5 {
            let activity = Activity {
                rel: format!("f{index}"),
                action: "uploaded".into(),
                size: index,
                finished_at: 100 + index,
            };
            insert_activity(&db, first.id, &activity).unwrap();
        }
        insert_activity(
            &db,
            second.id,
            &Activity {
                rel: "other".into(),
                action: "downloaded".into(),
                size: 1,
                finished_at: 1,
            },
        )
        .unwrap();
        let rels = |id| {
            recent_activity(&db, id, 10)
                .unwrap()
                .into_iter()
                .map(|activity| activity.rel)
                .collect::<Vec<_>>()
        };
        assert_eq!(rels(first.id), ["f4", "f3", "f2", "f1", "f0"]);
        prune_activity(&db, first.id, 2).unwrap();
        assert_eq!(rels(first.id), ["f4", "f3"]);
        assert_eq!(rels(second.id), ["other"]);
    }

    #[test]
    fn journal_entries_in_unknown_formats_are_refused() {
        let db = memory_db();
        exec(
            &db,
            "INSERT INTO operations (id, sync_dir_id, pair_uuid, rel_path, action, state, payload, created_at, updated_at)
             VALUES ('x', 1, 'p', 'a', 'upload', 'planned', '{\"v\":99}', 0, 0)",
            vec![],
        )
        .unwrap();
        assert!(incomplete_operations(&db, None).is_err());
    }
}
