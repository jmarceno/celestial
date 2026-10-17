pub use sea_orm_migration::prelude::*;

mod m20220101_000001_create_table;
mod m20230207_204909_sync_dirs_remove_slash_suffix;
mod m20230220_215840_remote_sync_items_fix;
mod m20261007_000001_sync_items_unique_paths;
mod m20261007_000002_sync_state;
mod m20261007_000003_internal_dir;
mod m20261008_000001_sync_activity;
mod m20261013_000001_paused_pairs;
mod m20261013_000002_dismissed_items;
mod m20261017_000001_keep_versions;

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
            Box::new(m20261007_000003_internal_dir::Migration),
            Box::new(m20261008_000001_sync_activity::Migration),
            Box::new(m20261013_000001_paused_pairs::Migration),
            Box::new(m20261013_000002_dismissed_items::Migration),
            Box::new(m20261017_000001_keep_versions::Migration),
        ]
    }
}
