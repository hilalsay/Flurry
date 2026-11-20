//! Window geometry on Wayland. Wayland doesn't tell apps where other
//! windows are or where the pointer is, so each compositor is asked in its
//! own way: Sway and Hyprland over their IPC sockets, KDE Plasma through a
//! small KWin script. Elsewhere (`none`) snow only piles at the screen
//! bottom.
//!
//! A provider runs in its own thread and publishes [`Snapshot`]s; the
//! backend picks up the latest one on each dispatch.

pub mod hyprland;
pub mod kwin;
pub mod none;
pub mod sway;

use std::sync::{Arc, Mutex};

use crate::geom::{AppWindow, Rect};

/// What the compositor told us, in global logical coordinates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Visible app windows on the current workspace(s), bottom-most first.
    pub windows: Vec<AppWindow>,
    /// Windows that exist but are hidden (minimized, other workspace).
    pub hidden: Vec<u64>,
    /// Per monitor, the area left for windows (outside panels), when known.
    pub work_areas: Vec<Rect>,
    pub active: Option<u64>,
    /// The focused window is fullscreen / any visible window is.
    pub fullscreen: bool,
    pub any_fullscreen: bool,
    /// The pointer, when the compositor tells (Hyprland, KDE).
    pub cursor: Option<(f32, f32)>,
}

/// The latest snapshot, shared between a provider thread and the backend.
#[derive(Debug, Clone, Default)]
pub struct Shared(Arc<Mutex<Latest>>);

#[derive(Debug, Default)]
struct Latest {
    snapshot: Snapshot,
    /// Bumped on every change, so the backend can tell what's new.
    version: u64,
}

impl Shared {
    /// Publish `s` if it differs from the current snapshot.
    pub fn publish(&self, s: Snapshot) {
        let Ok(mut l) = self.0.lock() else { return };
        if l.snapshot != s {
            l.snapshot = s;
            l.version += 1;
        }
    }

    /// Change only the cursor (it moves much more often than windows).
    pub fn set_cursor(&self, cursor: Option<(f32, f32)>) {
        let Ok(mut l) = self.0.lock() else { return };
        l.snapshot.cursor = cursor;
    }

    /// The snapshot and its version.
    pub fn get(&self) -> (Snapshot, u64) {
        self.0
            .lock()
            .map(|l| (l.snapshot.clone(), l.version))
            .unwrap_or_default()
    }

    pub fn cursor(&self) -> Option<(f32, f32)> {
        self.0.lock().ok().and_then(|l| l.snapshot.cursor)
    }
}

/// Which compositor we are on, from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sway,
    Hyprland,
    Kwin,
    None,
}

pub fn detect() -> Kind {
    let var = |k: &str| std::env::var(k).unwrap_or_default().to_ascii_lowercase();
    if std::env::var_os("SWAYSOCK").is_some() {
        Kind::Sway
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        Kind::Hyprland
    } else if var("XDG_CURRENT_DESKTOP").contains("kde") || var("KDE_FULL_SESSION") == "true" {
        Kind::Kwin
    } else {
        Kind::None
    }
}

/// A running provider; dropping it stops the thread (and, on KDE, unloads
/// the KWin script).
pub struct Provider {
    pub kind: Kind,
    pub shared: Shared,
    _guard: Option<Box<dyn Send>>,
}

/// Start the provider for this compositor. Falls back to `none` (with a
/// log line) if it can't start.
pub fn start() -> Provider {
    let kind = detect();
    let shared = Shared::default();
    let started = match kind {
        Kind::Sway => sway::start(shared.clone()),
        Kind::Hyprland => hyprland::start(shared.clone()),
        Kind::Kwin => kwin::start(shared.clone()),
        Kind::None => Ok(None),
    };
    match started {
        Ok(guard) => {
            log::info!("window info: {kind:?}");
            Provider {
                kind,
                shared,
                _guard: guard,
            }
        }
        Err(e) => {
            log::warn!(
                "window info from {kind:?} unavailable ({e:#}); snow will pile only at the \
                 bottom of the screen"
            );
            Provider {
                kind: Kind::None,
                shared,
                _guard: None,
            }
        }
    }
}

/// Stable 64-bit id from a compositor's window id string (Hyprland addresses,
/// KWin UUIDs).
pub fn id_of(s: &str) -> u64 {
    let hex: String = s.chars().filter(char::is_ascii_hexdigit).collect();
    if !hex.is_empty() && hex.len() <= 16 {
        if let Ok(v) = u64::from_str_radix(&hex, 16) {
            return v;
        }
    }
    // FNV-1a.
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_versions_only_change_on_change() {
        let s = Shared::default();
        let (_, v0) = s.get();
        s.publish(Snapshot::default());
        assert_eq!(s.get().1, v0, "same snapshot: no change");
        s.publish(Snapshot {
            active: Some(3),
            ..Snapshot::default()
        });
        assert_eq!(s.get().1, v0 + 1);
        s.set_cursor(Some((1.0, 2.0)));
        assert_eq!(s.cursor(), Some((1.0, 2.0)));
        assert_eq!(
            s.get().1,
            v0 + 1,
            "cursor moves don't count as window changes"
        );
    }

    #[test]
    fn ids_from_strings() {
        assert_eq!(id_of("0x55d1f0a2b3c0"), 0x55d1_f0a2_b3c0);
        let uuid = "{6a2b1c3d-0000-4000-8000-123456789abc}";
        assert_eq!(id_of(uuid), id_of(uuid));
        assert_ne!(id_of(uuid), id_of("{6a2b1c3d-0000-4000-8000-123456789abd}"));
    }
}
