use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

use flurry::cli::{Cli, Command};
use flurry::config::Config;
use flurry::ipc;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();

    let config_path = match cli.config.clone() {
        Some(p) => p,
        None => Config::default_path().context("cannot determine config directory")?,
    };

    match cli.command {
        None => run_daemon(&cli, &config_path),
        Some(Command::RenderTest { frames, out, seed }) => {
            let config = Config::load(&config_path)?;
            flurry::render_test::run(&config, frames, &out, seed)
        }
        Some(cmd) => send_command(cmd),
    }
}

fn run_daemon(cli: &Cli, config_path: &Path) -> Result<()> {
    if cli.install_autostart {
        anyhow::bail!("--install-autostart is not implemented yet (phase 7)");
    }
    let config = Config::load(config_path)?;
    log::debug!("config from {}: {config:?}", config_path.display());
    flurry::app::run(
        config,
        &flurry::app::RunOptions {
            backend: cli.backend,
            no_compositor_check: cli.no_compositor_check,
            config_path: config_path.to_owned(),
            socket_path: ipc::socket_path(),
        },
    )
}

fn send_command(cmd: Command) -> Result<()> {
    let cmd = match cmd {
        Command::Toggle => ipc::Command::Toggle,
        Command::Clear => ipc::Command::Clear,
        Command::Intensity { level } => ipc::Command::Intensity(level),
        Command::Fps { fps } => ipc::Command::Fps(fps),
        Command::Play => ipc::Command::Play,
        Command::Status => ipc::Command::Status,
        Command::Reload => ipc::Command::Reload,
        Command::Quit => ipc::Command::Quit,
        Command::RenderTest { .. } => unreachable!("handled in main"),
    };
    let answer = ipc::send(&ipc::socket_path(), cmd)?;
    println!("{answer}");
    if answer.starts_with("error") {
        std::process::exit(1);
    }
    Ok(())
}
