use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Content-aware synchronization state.
///
/// Existing history rows only hold whole-second timestamps. They keep them and
/// become `needs_verification`: the next pass compares contents before trusting
/// them, and never treats them as a verified common baseline.
///
/// Recovery records are keyed by the pair's stable identity rather than by a
/// foreign key, so stopping a pair or removing an account cannot discard the
/// inventory of preserved versions.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        for sql in [
            // Last common verified contents and last observations of each side.
            "ALTER TABLE sync_items ADD COLUMN state TEXT NOT NULL DEFAULT 'needs_verification';",
            "ALTER TABLE sync_items ADD COLUMN is_dir INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN size INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN hash_type TEXT;",
            "ALTER TABLE sync_items ADD COLUMN hash TEXT;",
            "ALTER TABLE sync_items ADD COLUMN local_mtime_nsec INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN local_ctime INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN local_ctime_nsec INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN local_inode INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN local_device INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN remote_size INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN remote_mtime_nsec INTEGER;",
            "ALTER TABLE sync_items ADD COLUMN remote_hash TEXT;",
            "ALTER TABLE sync_items ADD COLUMN remote_id TEXT;",
            "ALTER TABLE sync_items ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;",
            "ALTER TABLE sync_items ADD COLUMN verified_at INTEGER;",
            // Stable pair identity for recovery storage, and the pair generation.
            "ALTER TABLE sync_dirs ADD COLUMN pair_uuid TEXT;",
            "ALTER TABLE sync_dirs ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;",
            "UPDATE sync_dirs SET pair_uuid = lower(hex(randomblob(16))) WHERE pair_uuid IS NULL;",
            "CREATE TABLE IF NOT EXISTS item_states (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sync_dir_id INTEGER NOT NULL,
                rel_path TEXT NOT NULL,
                state TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt INTEGER,
                reason TEXT NOT NULL DEFAULT '',
                updated_at INTEGER NOT NULL,
                UNIQUE (sync_dir_id, rel_path)
            );",
            "CREATE TABLE IF NOT EXISTS conflicts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sync_dir_id INTEGER NOT NULL,
                pair_uuid TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                detail TEXT NOT NULL,
                conflict_copy TEXT,
                op_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                acknowledged INTEGER NOT NULL DEFAULT 0,
                UNIQUE (op_id)
            );",
            "CREATE TABLE IF NOT EXISTS operations (
                id TEXT PRIMARY KEY,
                sync_dir_id INTEGER NOT NULL,
                pair_uuid TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                action TEXT NOT NULL,
                state TEXT NOT NULL,
                payload TEXT NOT NULL,
                error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );",
            "CREATE TABLE IF NOT EXISTS recovery_items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                pair_uuid TEXT NOT NULL,
                remote_name TEXT NOT NULL,
                local_root TEXT NOT NULL,
                remote_root TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                side TEXT NOT NULL,
                stored_path TEXT NOT NULL,
                reason TEXT NOT NULL,
                op_id TEXT NOT NULL,
                size INTEGER,
                hash_type TEXT,
                hash TEXT,
                mtime INTEGER,
                created_at INTEGER NOT NULL,
                restored_at INTEGER,
                pruned_at INTEGER
            );",
            "CREATE INDEX IF NOT EXISTS recovery_items_by_item
                ON recovery_items (pair_uuid, rel_path, created_at);",
            "CREATE TABLE IF NOT EXISTS remote_capabilities (
                remote_name TEXT PRIMARY KEY,
                hashes TEXT NOT NULL,
                precision_ns INTEGER NOT NULL,
                case_insensitive INTEGER NOT NULL,
                duplicate_files INTEGER NOT NULL,
                partial_uploads INTEGER NOT NULL,
                server_move INTEGER NOT NULL,
                server_copy INTEGER NOT NULL,
                empty_dirs INTEGER NOT NULL,
                set_modtime INTEGER NOT NULL,
                list_r INTEGER NOT NULL,
                trash TEXT,
                rclone_version TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );",
        ] {
            db.execute(Statement::from_string(backend, sql.to_owned()))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Downgrading requires the database backup taken before this
        // migration ran; recovery assets must be kept.
        Err(DbErr::Migration(
            "Restore the database backup made before upgrading instead".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::migrations::{Migrator, MigratorTrait};
    use crate::util;
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, JsonValue, Statement};

    #[test]
    fn existing_history_needs_verification_and_pairs_get_identities() {
        let db = util::await_future(Database::connect("sqlite::memory:")).unwrap();
        util::await_future(Migrator::up(&db, Some(4))).unwrap();
        let backend = db.get_database_backend();
        let run = |sql: &str| {
            util::await_future(db.execute(Statement::from_string(backend, sql.to_owned())))
        };
        run("INSERT INTO remotes (name) VALUES ('r');").unwrap();
        run("INSERT INTO sync_dirs (remote_id, local_path, remote_path) VALUES (1, '/l', 'r');")
            .unwrap();
        run("INSERT INTO sync_dirs (remote_id, local_path, remote_path) VALUES (1, '/m', 'm');")
            .unwrap();
        run("INSERT INTO sync_items (sync_dir_id, local_path, remote_path, last_local_timestamp, last_remote_timestamp) VALUES (1, '/l/a', 'r/a', -5, 4000000000);")
            .unwrap();
        util::await_future(Migrator::up(&db, None)).unwrap();
        let rows = util::await_future(
            JsonValue::find_by_statement(Statement::from_string(
                backend,
                "SELECT state, last_local_timestamp, last_remote_timestamp, hash FROM sync_items;"
                    .to_owned(),
            ))
            .all(&db),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "needs_verification");
        assert_eq!(rows[0]["last_local_timestamp"], -5);
        assert_eq!(rows[0]["last_remote_timestamp"], 4_000_000_000i64);
        assert!(rows[0]["hash"].is_null());
        let pairs = util::await_future(
            JsonValue::find_by_statement(Statement::from_string(
                backend,
                "SELECT DISTINCT pair_uuid FROM sync_dirs WHERE pair_uuid IS NOT NULL;".to_owned(),
            ))
            .all(&db),
        )
        .unwrap();
        assert_eq!(pairs.len(), 2);
    }
}
