use clap::Parser;
use celeste::cli::{Cli, Commands};

fn main() {
    celeste::init_tracing();
    let cli = Cli::parse();
    let background = celeste::cli::background_requested(&cli);
    let code = match cli.command {
        None | Some(Commands::App) | Some(Commands::RunGui { .. }) => celeste::ui::run(background),
        Some(cmd) => celeste::cli::run(cmd),
    };
    std::process::exit(code);
}
