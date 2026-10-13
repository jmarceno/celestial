use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// A pair can wait before its first pass, so its exclusions can be edited
/// before anything is transferred. Existing pairs keep syncing.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            "ALTER TABLE sync_dirs ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;".to_owned(),
        ))
        .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Migration(
            "Restore the database backup made before upgrading instead".into(),
        ))
    }
}
