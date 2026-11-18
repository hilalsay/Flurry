//! Command-line interface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::config::Intensity;
pub use crate::platform::BackendKind;

#[derive(Debug, Parser)]
#[command(
    name = "flurry",
    version,
    about = "Snow falling over your Linux desktop"
)]
pub struct Cli {
    /// Display backend to use (default: auto-detect).
    #[arg(long, value_enum, global = true)]
    pub backend: Option<BackendKind>,

    /// Config file (default: ~/.config/flurry/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// X11: run even when no compositing manager is detected.
    #[arg(long)]
    pub no_compositor_check: bool,

    /// Write ~/.config/autostart/flurry.desktop and exit.
    #[arg(long)]
    pub install_autostart: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Pause or resume the running instance.
    Toggle,
    /// Remove all settled snow.
    Clear,
    /// Change snowfall intensity.
    Intensity {
        #[arg(value_enum)]
        level: Intensity,
    },
    /// Toggle play mode (lets the overlay receive the mouse on Wayland).
    Play,
    /// Re-read the config file.
    Reload,
    /// Stop the running instance.
    Quit,
    /// Run the simulation headless and write PNG frames (development aid).
    #[command(hide = true)]
    RenderTest {
        #[arg(long, default_value_t = 300)]
        frames: u32,
        #[arg(long, default_value = "frames")]
        out: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_intensity() {
        let cli = Cli::try_parse_from(["flurry", "intensity", "heavy"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Intensity {
                level: Intensity::Heavy
            })
        ));
    }

    #[test]
    fn no_subcommand_means_daemon() {
        let cli = Cli::try_parse_from(["flurry", "--backend", "x11"]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.backend, Some(BackendKind::X11));
    }
}
