//! Command line. No subcommand launches the Qt Quick window.

use clap::{Parser, Subcommand};

use crate::platform::desktop;

#[derive(Parser, Debug)]
#[command(name = "celestial", version, about = "Folder sync for cloud storage")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Start in the background. The tray icon is still created.
    #[arg(long, global = true)]
    pub background: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start the GUI (default).
    App,
    /// Add the app-menu entry (and optionally start at login) for this binary.
    Install {
        #[arg(long)]
        autostart: bool,
    },
    /// Remove the app-menu entry, autostart entry and icon. Configuration,
    /// synchronization history and synced files are kept.
    Uninstall,
    /// Print config / runtime paths.
    Paths,
}

pub fn run(cmd: Commands) -> i32 {
    match cmd {
        Commands::App => unreachable!("handled in main"),
        Commands::Install { autostart } => {
            let exe = desktop::resolve_binary_path();
            // Keep login autostart if it was enabled under the former name.
            let legacy_autostart = desktop::remove_legacy_entries();
            let mut ok = report(desktop::write_menu_entry(&exe), "menu entry");
            if autostart || legacy_autostart {
                ok &= report(desktop::write_autostart(&exe), "autostart entry");
            }
            if ok { 0 } else { 1 }
        }
        Commands::Uninstall => {
            desktop::remove_legacy_entries();
            let mut ok = report_removed(desktop::remove_menu_entry(), "menu entry")
                & report_removed(desktop::remove_autostart(), "autostart entry")
                & report_removed(desktop::remove_icon(), "icon");
            // Keep the next start from adding the menu entry back.
            if let Err(err) = desktop::remember_first_run() {
                eprintln!("could not record the uninstall: {err}");
                ok = false;
            }
            println!(
                "the executable ({}) and configuration ({}) were left in place",
                desktop::resolve_binary_path().display(),
                crate::util::get_config_dir().display()
            );
            if ok { 0 } else { 1 }
        }
        Commands::Paths => {
            println!("config:    {}", crate::util::get_config_dir().display());
            println!("runtime:   {}", crate::util::runtime_dir().display());
            println!("menu:      {}", desktop::menu_entry_path().display());
            println!("autostart: {}", desktop::autostart_path().display());
            println!("binary:    {}", desktop::resolve_binary_path().display());
            0
        }
    }
}

fn report(res: Result<std::path::PathBuf, String>, what: &str) -> bool {
    match res {
        Ok(path) => {
            println!("wrote {what}: {}", path.display());
            true
        }
        Err(err) => {
            eprintln!("could not write {what}: {err}");
            false
        }
    }
}

fn report_removed(res: Result<Option<std::path::PathBuf>, String>, what: &str) -> bool {
    match res {
        Ok(Some(path)) => {
            println!("removed {what}: {}", path.display());
            true
        }
        Ok(None) => true,
        Err(err) => {
            eprintln!("could not remove {what}: {err}");
            false
        }
    }
}
