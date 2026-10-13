pub mod cli;
pub mod entities;
pub mod exit;
pub mod login;
pub mod migrations;
pub mod platform;
pub mod rclone;
pub mod single_instance;
pub mod sync;
pub mod ui;
pub mod util;

pub use util::{APP_ID, APP_NAME};

pub fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    // Starts, passes and their failures are logged by default: they are what
    // a log needs to explain a crash or a stalled synchronization.
    let spec = std::env::var("CELESTIAL_LOG")
        .unwrap_or_else(|_| "warn,celestial::ui=info,celestial::sync::engine=info".to_string());
    let filter = EnvFilter::try_new(&spec).unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
