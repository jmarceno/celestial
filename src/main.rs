use celeste::cli::{Cli, Commands};
use clap::Parser;

fn main() {
    celeste::init_tracing();
    let cli = Cli::parse();
    let background = cli.background;
    let code = match cli.command {
        None | Some(Commands::App) => celeste::ui::run(background),
        Some(cmd) => celeste::cli::run(cmd),
    };
    std::process::exit(code);
}
