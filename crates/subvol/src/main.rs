mod cli;
mod config;
pub mod daemon;
mod daemon_cmd;
mod daemon_server;
mod daemon_volume;
mod vol_cmd;

use cli::Command;
use config::SubvolmountdConfig;

fn main() {
    let cli = cli::Cli::parse_args();

    let config_path = SubvolmountdConfig::default_config_path();
    let config = match SubvolmountdConfig::load_or_default(&config_path) {
        Ok((config, _used_default)) => config,
        Err(e) => {
            eprintln!(
                "error: failed to load config {}: {e}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };

    match cli.command {
        Command::Fuse { mountpoint, foreground } => {
            vol_cmd::execute_fuse_mount(&config, &mountpoint, foreground);
        }
        Command::Nbd { foreground } => {
            daemon_cmd::execute_nbd_start(&config, &config_path, foreground);
        }
    }
}
