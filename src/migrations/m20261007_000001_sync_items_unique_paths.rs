use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Older releases could record the same item twice. Synchronization updates
/// the first row while the change probe could read the stale copy, so keep the
/// oldest row and let the database reject further duplicates.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        for sql in [
            "DELETE FROM sync_items WHERE id NOT IN (
                SELECT MIN(id) FROM sync_items GROUP BY sync_dir_id, local_path, remote_path
            );",
            "CREATE UNIQUE INDEX IF NOT EXISTS sync_items_unique_paths
                ON sync_items (sync_dir_id, local_path, remote_path);",
        ] {
            db.execute(Statement::from_string(backend, sql.to_owned()))
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let stmt = Statement::from_string(
            manager.get_database_backend(),
            "DROP INDEX IF EXISTS sync_items_unique_paths;".to_owned(),
        );
        manager.get_connection().execute(stmt).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use crate::migrations::{Migrator, MigratorTrait};
    use crate::util;
    use sea_orm::{ConnectionTrait, Database, FromQueryResult, JsonValue, Statement};

    #[test]
    fn keeps_the_first_row_of_each_duplicate_and_rejects_new_ones() {
        let db = util::await_future(Database::connect("sqlite::memory:")).unwrap();
        util::await_future(Migrator::up(&db, Some(3))).unwrap();
        let backend = db.get_database_backend();
        let run = |sql: &str| {
            util::await_future(db.execute(Statement::from_string(backend, sql.to_owned())))
        };
        run("INSERT INTO remotes (name) VALUES ('r');").unwrap();
        run("INSERT INTO sync_dirs (remote_id, local_path, remote_path) VALUES (1, '/l', 'r');")
            .unwrap();
        for (path, stamp) in [("/l/a", 10), ("/l/a", 20), ("/l/b", 30)] {
            run(&format!(
                "INSERT INTO sync_items (sync_dir_id, local_path, remote_path, last_local_timestamp, last_remote_timestamp) VALUES (1, '{path}', 'r{}', {stamp}, {stamp});",
                &path[2..]
            ))
            .unwrap();
        }
        util::await_future(Migrator::up(&db, None)).unwrap();
        let rows = util::await_future(
            JsonValue::find_by_statement(Statement::from_string(
                backend,
                "SELECT local_path, last_local_timestamp FROM sync_items ORDER BY id;".to_owned(),
            ))
            .all(&db),
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["local_path"], "/l/a");
        assert_eq!(rows[0]["last_local_timestamp"], 10);
        assert!(
            run("INSERT INTO sync_items (sync_dir_id, local_path, remote_path, last_local_timestamp, last_remote_timestamp) VALUES (1, '/l/a', 'r/a', 1, 1);")
                .is_err()
        );
    }
}
