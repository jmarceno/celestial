use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// Recently synchronized files, shown per pair and from the tray.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        for sql in [
            "CREATE TABLE sync_activity (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sync_dir_id INTEGER NOT NULL,
                rel TEXT NOT NULL,
                action TEXT NOT NULL,
                size INTEGER NOT NULL,
                finished_at INTEGER NOT NULL
            );",
            "CREATE INDEX sync_activity_pair ON sync_activity (sync_dir_id, id);",
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
