//! Display backends: everything that talks to X11 or Wayland.
//!
//! The trait grows with the phases (windows and obstacles in 5–6, cursor
//! and input passthrough in 7) instead of carrying unused stubs.

pub mod wayland;
pub mod wininfo;
pub mod x11;

use std::time::Duration;

use crate::config::Monitors;
use crate::geom::OutputInfo;
use crate::render::{Scene, SpriteSheet};
use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum BackendKind {
    X11,
    Wayland,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendEvent {
    /// Monitors were added, removed or moved: re-read `outputs()`.
    OutputsChanged,
}

#[derive(Debug, Clone)]
pub struct BackendOptions {
    /// X11: skip the "is a compositor running?" check.
    pub no_compositor_check: bool,
    pub monitors: Monitors,
}

pub trait Backend {
    fn name(&self) -> &'static str;

    /// The overlaid monitors, in global coordinates. Indices match `present`.
    fn outputs(&self) -> &[OutputInfo];

    /// Show the next frame on output `output`. `scene` is incremental: it
    /// clears and redraws only what changed since the previous scene for this
    /// output. `sprites` never changes, so backends may upload it once.
    /// After e.g. an Expose the backend must redraw fully on its own
    /// (`scene.draws` always lists every visible sprite).
    fn present(&mut self, output: usize, scene: &Scene, sprites: &SpriteSheet) -> Result<()>;

    /// Handle pending events, waiting at most `timeout` for the first one.
    fn dispatch(&mut self, timeout: Duration) -> Result<Vec<BackendEvent>>;
}

/// Pick and start a backend (PLAN.md §4 "Backend selection").
pub fn select(requested: Option<BackendKind>, opts: &BackendOptions) -> Result<Box<dyn Backend>> {
    let has_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let has_x11 = std::env::var_os("DISPLAY").is_some();
    let kind = match requested {
        Some(k) => k,
        None if has_wayland && has_x11 => {
            log::warn!(
                "Wayland session detected, but the Wayland backend isn't built yet; \
                 using X11 through XWayland (snow may not stay above native Wayland windows)"
            );
            BackendKind::X11
        }
        None if has_x11 => BackendKind::X11,
        None if has_wayland => {
            bail!("Wayland session without XWayland: the Wayland backend isn't built yet")
        }
        None => bail!("no display found: neither WAYLAND_DISPLAY nor DISPLAY is set"),
    };
    log::debug!("backend: {kind:?}");
    match kind {
        BackendKind::X11 => Ok(Box::new(x11::X11Backend::new(opts)?)),
        BackendKind::Wayland => bail!("the Wayland backend isn't built yet; use --backend x11"),
    }
}

/// Keep only the outputs selected by `monitors`. Falls back to all of them
/// (with a warning) if none match, so a renamed connector doesn't hide the snow.
pub fn filter_outputs(all: Vec<OutputInfo>, monitors: &Monitors) -> Vec<OutputInfo> {
    let chosen: Vec<_> = all
        .iter()
        .filter(|o| monitors.includes(&o.name))
        .cloned()
        .collect();
    if chosen.is_empty() && !all.is_empty() {
        let names: Vec<_> = all.iter().map(|o| o.name.as_str()).collect();
        log::warn!("no monitor matches `monitors` in the config; available: {names:?}. Using all.");
        return all;
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;

    fn out(name: &str) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            rect: Rect::new(0, 0, 10, 10),
            scale: 1.0,
        }
    }

    #[test]
    fn filter_outputs_by_name_or_fall_back() {
        let all = vec![out("DP-1"), out("eDP-1")];
        let list = Monitors::List(vec!["eDP-1".into()]);
        assert_eq!(filter_outputs(all.clone(), &list), vec![out("eDP-1")]);
        let none = Monitors::List(vec!["HDMI-1".into()]);
        assert_eq!(filter_outputs(all.clone(), &none), all);
    }
}
