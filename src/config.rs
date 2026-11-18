//! User configuration: `~/.config/flurry/config.toml`.
//!
//! Every field has a default, so an empty or partial file is valid.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Intensity {
    Light,
    Medium,
    Heavy,
}

impl Intensity {
    pub fn flake_count(self) -> u32 {
        match self {
            Intensity::Light => 150,
            Intensity::Medium => 300,
            Intensity::Heavy => 600,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CarryModifier {
    None,
    Alt,
    Super,
}

/// Where falling flakes are drawn relative to app windows (PLAN.md §4 "Layering").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layering {
    /// Two-window on X11, mask on Wayland.
    Auto,
    /// Flakes window below apps, piles overlay on top.
    TwoWindow,
    /// Everything on the top overlay, flakes masked by window rects.
    Mask,
    /// Flakes below apps, no piles overlay.
    Desktop,
    /// Everything on top of all windows (the Phase 2 behaviour).
    Front,
}

/// `monitors = "all"` or `monitors = ["DP-1", "HDMI-A-1"]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Monitors {
    All(AllMarker),
    List(Vec<String>),
}

/// Only accepts the literal string `"all"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllMarker {
    All,
}

impl Monitors {
    pub fn includes(&self, output_name: &str) -> bool {
        match self {
            Monitors::All(_) => true,
            Monitors::List(names) => names.iter().any(|n| n == output_name),
        }
    }
}

/// The signature features (PLAN.md §12). `[features]` in the config file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Features {
    /// Windows whose programs use a lot of CPU melt their snow faster.
    pub hot_apps_melt: bool,
    /// Icicles grow under snowy windows and on the panel.
    pub icicles: bool,
    /// Typing in a window shakes little clumps off its top.
    pub typing_shakes_snow: bool,
    /// Frost creeps in from the corners when the mouse is idle.
    pub frost: bool,
    /// Minutes without mouse movement before frost starts.
    pub frost_idle_minutes: f32,
    /// Opt-in: follow the real weather of `weather_city` (Open-Meteo).
    pub real_weather: bool,
    /// City name for real weather mode, e.g. "Istanbul". Never auto-detected.
    pub weather_city: String,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            hot_apps_melt: true,
            icicles: true,
            typing_shakes_snow: true,
            frost: true,
            frost_idle_minutes: 3.0,
            real_weather: false,
            weather_city: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub intensity: Intensity,
    /// Overrides `intensity` when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flake_count: Option<u32>,
    pub fall_speed: f32,
    pub wind: f32,
    /// Maximum pile height in px.
    pub max_pile_height: u32,
    pub melt_rate: f32,
    pub fps: u32,
    pub pile_on_windows: bool,
    pub interaction: bool,
    pub carry_modifier: CarryModifier,
    pub auto_pause_fullscreen: bool,
    pub monitors: Monitors,
    pub layering: Layering,
    /// Global toggle hotkey, e.g. `"super+shift+s"`; empty disables it.
    pub hotkey: String,
    pub features: Features,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            intensity: Intensity::Medium,
            flake_count: None,
            fall_speed: 1.0,
            wind: 1.0,
            max_pile_height: 40,
            melt_rate: 1.0,
            fps: 30,
            pile_on_windows: true,
            interaction: true,
            carry_modifier: CarryModifier::Alt,
            auto_pause_fullscreen: true,
            monitors: Monitors::All(AllMarker::All),
            layering: Layering::Auto,
            hotkey: "super+shift+s".into(),
            features: Features::default(),
        }
    }
}

impl Config {
    /// Number of flakes to simulate: explicit `flake_count`, else from `intensity`.
    pub fn effective_flake_count(&self) -> u32 {
        self.flake_count
            .unwrap_or_else(|| self.intensity.flake_count())
    }

    /// `$XDG_CONFIG_HOME/flurry/config.toml`, if a home directory is known.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "flurry").map(|d| d.config_dir().join("config.toml"))
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from `path`. A missing file yields defaults; a malformed one is an error.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text)
                .with_context(|| format!("invalid config file {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::debug!("no config at {}, using defaults", path.display());
                Ok(Self::default())
            }
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (1..=240).contains(&self.fps),
            "fps must be 1..=240, got {}",
            self.fps
        );
        anyhow::ensure!(
            self.fall_speed >= 0.0,
            "fall_speed must be >= 0, got {}",
            self.fall_speed
        );
        anyhow::ensure!(
            self.melt_rate >= 0.0,
            "melt_rate must be >= 0, got {}",
            self.melt_rate
        );
        anyhow::ensure!(
            self.flake_count.is_none_or(|n| n <= 20_000),
            "flake_count must be <= 20000"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn plan_example_parses() {
        let text = r#"
            intensity = "heavy"
            flake_count = 300
            fall_speed = 1.0
            wind = 1.0
            max_pile_height = 40
            melt_rate = 1.0
            fps = 60
            pile_on_windows = true
            interaction = true
            carry_modifier = "alt"
            auto_pause_fullscreen = true
            monitors = "all"
            layering = "two-window"
        "#;
        let cfg = Config::from_toml(text).unwrap();
        assert_eq!(cfg.intensity, Intensity::Heavy);
        assert_eq!(cfg.effective_flake_count(), 300);
        assert_eq!(cfg.layering, Layering::TwoWindow);
    }

    #[test]
    fn features_table() {
        let cfg = Config::from_toml("[features]\nhot_apps_melt = false\n").unwrap();
        assert!(!cfg.features.hot_apps_melt);
        assert!(Config::default().features.hot_apps_melt);
        assert!(Config::from_toml("[features]\nnope = true\n").is_err());
    }

    #[test]
    fn intensity_sets_flake_count_when_unset() {
        let cfg = Config::from_toml(r#"intensity = "light""#).unwrap();
        assert_eq!(cfg.effective_flake_count(), 150);
    }

    #[test]
    fn monitor_list() {
        let cfg = Config::from_toml(r#"monitors = ["DP-1", "eDP-1"]"#).unwrap();
        assert!(cfg.monitors.includes("eDP-1"));
        assert!(!cfg.monitors.includes("HDMI-A-1"));
        assert!(Config::default().monitors.includes("anything"));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::from_toml(r#"monitors = "some""#).is_err());
        assert!(Config::from_toml("fps = 0").is_err());
        assert!(Config::from_toml(r#"carry_modifier = "ctrl""#).is_err());
        assert!(Config::from_toml("unknown_key = 1").is_err());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("flurry-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let cfg = Config {
            flake_count: Some(42),
            monitors: Monitors::List(vec!["DP-1".into()]),
            ..Config::default()
        };
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = Path::new("/nonexistent/flurry/config.toml");
        assert_eq!(Config::load(path).unwrap(), Config::default());
    }
}
