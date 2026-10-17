use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// How long each pair keeps preserved versions, in days; 0 keeps them until
/// the person deletes them. Existing pairs keep the former 30 days.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        db.execute(Statement::from_string(
            backend,
            "ALTER TABLE sync_dirs ADD COLUMN keep_versions_days INTEGER NOT NULL DEFAULT 30;"
                .to_owned(),
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
