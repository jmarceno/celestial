use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Per-pair limits for large folders: the largest file that keeps a preserved
/// copy, and how long the pair waits between synchronization passes.
///
/// Existing pairs keep a copy of files up to 100 MB, as before, and keep
/// synchronizing as changes are detected (`0`).
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        for sql in [
            "ALTER TABLE sync_dirs ADD COLUMN recovery_max_bytes INTEGER NOT NULL DEFAULT 100000000;",
            "ALTER TABLE sync_dirs ADD COLUMN sync_interval_secs INTEGER NOT NULL DEFAULT 0;",
        ] {
            db.execute(Statement::from_string(backend, sql.to_owned()))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
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
    fn existing_pairs_keep_a_copy_of_files_up_to_100_mb_and_sync_as_changes_arrive() {
        let db = util::await_future(Database::connect("sqlite::memory:")).unwrap();
        util::await_future(Migrator::up(&db, Some(10))).unwrap();
        let backend = db.get_database_backend();
        let run = |sql: &str| {
            util::await_future(db.execute(Statement::from_string(backend, sql.to_owned())))
        };
        run("INSERT INTO remotes (name) VALUES ('r');").unwrap();
        run("INSERT INTO sync_dirs (remote_id, local_path, remote_path) VALUES (1, '/l', 'r');")
            .unwrap();
        util::await_future(Migrator::up(&db, None)).unwrap();
        let rows = util::await_future(
            JsonValue::find_by_statement(Statement::from_string(
                backend,
                "SELECT recovery_max_bytes, sync_interval_secs FROM sync_dirs;".to_owned(),
            ))
            .all(&db),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["recovery_max_bytes"], 100_000_000);
        assert_eq!(rows[0]["sync_interval_secs"], 0);
    }
}
