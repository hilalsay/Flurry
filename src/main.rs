use anyhow::{Context, Result};
use clap::Parser;

use flurry::cli::{Cli, Command};
use flurry::config::Config;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    let config_path = match cli.config.clone() {
        Some(p) => p,
        None => Config::default_path().context("cannot determine config directory")?,
    };

    match cli.command {
        None => run_daemon(&cli, &config_path),
        Some(cmd) => run_command(cmd),
    }
}

fn run_daemon(cli: &Cli, config_path: &std::path::Path) -> Result<()> {
    if cli.install_autostart {
        anyhow::bail!("--install-autostart is not implemented yet (phase 8)");
    }
    let config = Config::load(config_path)?;
    log::debug!("config from {}: {config:?}", config_path.display());
    log::debug!("requested backend: {:?}", cli.backend);
    anyhow::bail!("no display backend implemented yet (phases 2–3)")
}

fn run_command(cmd: Command) -> Result<()> {
    match cmd {
        Command::RenderTest { .. } => {
            anyhow::bail!("render-test is not implemented yet (phase 1)")
        }
        other => anyhow::bail!("{other:?}: IPC is not implemented yet (phase 4)"),
    }
}
