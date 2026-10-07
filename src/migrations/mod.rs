pub use sea_orm_migration::prelude::*;

mod m20220101_000001_create_table;
mod m20230207_204909_sync_dirs_remove_slash_suffix;
mod m20230220_215840_remote_sync_items_fix;
mod m20261007_000001_sync_items_unique_paths;
mod m20261007_000002_sync_state;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20220101_000001_create_table::Migration),
            Box::new(m20230207_204909_sync_dirs_remove_slash_suffix::Migration),
            Box::new(m20230220_215840_remote_sync_items_fix::Migration),
            Box::new(m20261007_000001_sync_items_unique_paths::Migration),
            Box::new(m20261007_000002_sync_state::Migration),
        ]
    }
}
