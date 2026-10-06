//! Command line. No subcommand launches the GUI, matching the old desktop entry.

use clap::{Parser, Subcommand};

use crate::platform::desktop;

#[derive(Parser, Debug)]
#[command(name = "celeste", version, about = "Folder sync for cloud storage")]
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
    /// Previous entry point used by the panic wrapper. Same as launching the GUI.
    RunGui {
        #[arg(long)]
        background: bool,
    },
    /// Add the app-menu entry (and optionally start at login) for this binary.
    Install {
        #[arg(long)]
        autostart: bool,
    },
    /// Remove the app-menu and autostart entries.
    Uninstall,
    /// Print config / runtime paths.
    Paths,
}

pub fn background_requested(cli: &Cli) -> bool {
    match &cli.command {
        Some(Commands::RunGui { background }) => *background || cli.background,
        _ => cli.background,
    }
}

pub fn run(cmd: Commands) -> i32 {
    match cmd {
        Commands::App | Commands::RunGui { .. } => unreachable!("handled in main"),
        Commands::Install { autostart } => {
            let exe = desktop::resolve_binary_path();
            let mut ok = report(desktop::write_menu_entry(&exe), "menu entry");
            if autostart {
                ok &= report(desktop::write_autostart(&exe), "autostart entry");
            }
            if ok { 0 } else { 1 }
        }
        Commands::Uninstall => {
            let ok = report_removed(desktop::remove_menu_entry(), "menu entry")
                & report_removed(desktop::remove_autostart(), "autostart entry");
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
