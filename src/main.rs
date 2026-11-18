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
        Some(cmd) => run_command(cmd, &config_path),
    }
}

fn run_daemon(cli: &Cli, config_path: &std::path::Path) -> Result<()> {
    if cli.install_autostart {
        anyhow::bail!("--install-autostart is not implemented yet (phase 8)");
    }
    let config = Config::load(config_path)?;
    log::debug!("config from {}: {config:?}", config_path.display());
    flurry::app::run(
        &config,
        &flurry::app::RunOptions {
            backend: cli.backend,
            no_compositor_check: cli.no_compositor_check,
        },
    )
}

fn run_command(cmd: Command, config_path: &std::path::Path) -> Result<()> {
    match cmd {
        Command::RenderTest { frames, out, seed } => {
            let config = Config::load(config_path)?;
            flurry::render_test::run(&config, frames, &out, seed)
        }
        other => anyhow::bail!("{other:?}: IPC is not implemented yet (phase 4)"),
    }
}
