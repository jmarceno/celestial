use sea_orm::{ConnectionTrait, Statement};
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

/// The identity of each pair's internal folder (staging and recovery), so it
/// is excluded by what it is rather than by a name a person could reuse.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        for sql in [
            "ALTER TABLE sync_dirs ADD COLUMN internal_name TEXT;",
            "ALTER TABLE sync_dirs ADD COLUMN internal_dev INTEGER;",
            "ALTER TABLE sync_dirs ADD COLUMN internal_ino INTEGER;",
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
