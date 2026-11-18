//! Display backends: everything that talks to X11 or Wayland.
//!
//! The trait grows with the phases (windows and obstacles in 5–6, cursor
//! and input passthrough in 7) instead of carrying unused stubs.

pub mod wayland;
pub mod wininfo;
pub mod x11;

use std::time::Duration;

use crate::config::{Layering, Monitors};
use crate::geom::{AppWindow, CursorState, OutputInfo, Rect, WindowRect};
use crate::render::{ImageSheet, PileScene, Scene, SpriteSheet};
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
    /// The global toggle hotkey was pressed.
    Hotkey,
    /// Panels/docks moved, resized, appeared or vanished: re-read `obstacles()`.
    ObstaclesChanged,
    /// App windows moved, restacked, appeared, vanished or changed state:
    /// re-read `windows()` and `hidden_windows()`.
    WindowsChanged,
    /// Output `n`'s windows lost their content (e.g. Expose): redraw it in full.
    Redraw(usize),
    /// This many keys were pressed anywhere (counted, not identified).
    Keys(u32),
}

/// Everything to show on one output this frame.
pub struct Frame<'a> {
    /// Falling flakes (drawn behind app windows when layering allows).
    pub flakes: &'a Scene,
    /// Settled snow, drawn on top of everything.
    pub piles: &'a PileScene,
    pub sprites: &'a SpriteSheet,
    pub images: &'a ImageSheet,
}

/// A global key combination such as `super+shift+s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hotkey {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_: bool,
    /// A lowercase ASCII letter or digit.
    pub key: char,
}

impl Hotkey {
    /// Parse `"super+shift+s"`. An empty string means "no hotkey".
    pub fn parse(text: &str) -> Result<Option<Hotkey>> {
        let text = text.trim().to_ascii_lowercase();
        if text.is_empty() || text == "none" {
            return Ok(None);
        }
        let mut hk = Hotkey::default();
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let key = parts.pop().unwrap_or_default();
        for m in parts {
            match m {
                "shift" => hk.shift = true,
                "ctrl" | "control" => hk.ctrl = true,
                "alt" => hk.alt = true,
                "super" | "meta" | "win" => hk.super_ = true,
                _ => bail!("unknown modifier {m:?} in hotkey {text:?}"),
            }
        }
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_ascii_alphanumeric() => hk.key = c,
            _ => bail!("hotkey key must be one letter or digit, got {key:?}"),
        }
        Ok(Some(hk))
    }
}

impl std::fmt::Display for Hotkey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (on, name) in [
            (self.super_, "Super+"),
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
        ] {
            if on {
                f.write_str(name)?;
            }
        }
        write!(f, "{}", self.key.to_ascii_uppercase())
    }
}

#[derive(Debug, Clone)]
pub struct BackendOptions {
    /// X11: skip the "is a compositor running?" check.
    pub no_compositor_check: bool,
    pub monitors: Monitors,
    pub hotkey: Option<Hotkey>,
    pub layering: Layering,
}

pub trait Backend {
    fn name(&self) -> &'static str;

    /// The overlaid monitors, in global coordinates. Indices match `present`.
    fn outputs(&self) -> &[OutputInfo];

    /// Panels/docks, in global coordinates.
    fn obstacles(&self) -> &[WindowRect];

    /// Visible app windows on the current workspace, bottom-most first.
    fn windows(&self) -> &[AppWindow];

    /// Ids of app windows that exist but are hidden (minimized, other workspace).
    fn hidden_windows(&self) -> &[u64];

    /// Is the focused window fullscreen (a video, a game, a presentation)?
    fn fullscreen(&self) -> bool;

    /// The focused window, if known.
    fn active_window(&self) -> Option<u64>;

    /// Re-read monitors, panels and windows (e.g. after waking from sleep).
    /// Returns the events that produces.
    fn rescan(&mut self) -> Result<Vec<BackendEvent>>;

    /// Where the pointer is and which buttons/modifiers are down, if known.
    fn cursor(&mut self) -> Option<CursorState>;

    /// Catch clicks (instead of passing them through) inside `rects`
    /// (output-local) on output `output`'s top window. Empty = click-through.
    fn set_input_region(&mut self, output: usize, rects: &[Rect]) -> Result<()>;

    /// Show the next frame on output `output`. Scenes are incremental: they
    /// clear and redraw only what changed since the previous frame for this
    /// output. `sprites` never changes, so backends may upload it once.
    /// If a window loses its content the backend sends `Redraw`.
    fn present(&mut self, output: usize, frame: &Frame) -> Result<()>;

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
    fn hotkey_parsing() {
        let hk = Hotkey::parse("Super+Shift+S").unwrap().unwrap();
        assert!(hk.super_ && hk.shift && !hk.ctrl && !hk.alt);
        assert_eq!(hk.key, 's');
        assert_eq!(hk.to_string(), "Super+Shift+S");
        assert_eq!(Hotkey::parse("").unwrap(), None);
        assert!(Hotkey::parse("hyper+s").is_err());
        assert!(Hotkey::parse("super+space").is_err());
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
