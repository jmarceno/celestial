use celestial::cli::{Cli, Commands};
use clap::Parser;

fn main() {
    celestial::init_tracing();
    let cli = Cli::parse();
    let background = cli.background;
    let code = match cli.command {
        None | Some(Commands::App) => celestial::ui::run(background),
        Some(cmd) => celestial::cli::run(cmd),
    };
    std::process::exit(code);
}
