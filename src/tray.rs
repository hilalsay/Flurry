//! StatusNotifierItem tray icon via `ksni`. Cinnamon shows these through
//! `xapp-sn-watcher`. Menu clicks become [`Request`]s for the main loop; the
//! main loop pushes the current state back with [`Tray::update`].

use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem};
use ksni::{Icon, MenuItem, ToolTip};
use tiny_skia::{LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::config::Intensity;
use crate::ipc::{Command, Request};

/// What the menu shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayState {
    pub paused: bool,
    pub intensity: Intensity,
    pub fps: u32,
}

const SMOOTH_FPS: u32 = 60;
const DEFAULT_FPS: u32 = 30;
const INTENSITIES: [Intensity; 3] = [Intensity::Light, Intensity::Medium, Intensity::Heavy];

struct FlurryTray {
    state: TrayState,
    tx: Sender<Request>,
    icons: Vec<Icon>,
}

impl FlurryTray {
    fn send(&self, cmd: Command) {
        if self.tx.send(Request::fire(cmd)).is_err() {
            log::debug!("tray: daemon gone, dropping {cmd:?}");
        }
    }
}

impl ksni::Tray for FlurryTray {
    fn id(&self) -> String {
        "flurry".into()
    }

    fn title(&self) -> String {
        "Flurry".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: if self.state.paused {
                "Flurry (paused)"
            } else {
                "Flurry"
            }
            .into(),
            ..ToolTip::default()
        }
    }

    /// Left click toggles the snow.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.state.paused = !self.state.paused;
        self.send(Command::Toggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let selected = INTENSITIES
            .iter()
            .position(|&i| i == self.state.intensity)
            .unwrap_or(1);
        vec![
            StandardItem {
                label: if self.state.paused {
                    "Resume snow"
                } else {
                    "Pause snow"
                }
                .into(),
                activate: Box::new(|t: &mut Self| {
                    t.state.paused = !t.state.paused;
                    t.send(Command::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            RadioGroup {
                selected,
                select: Box::new(|t: &mut Self, i| {
                    let level = INTENSITIES[i.min(INTENSITIES.len() - 1)];
                    t.state.intensity = level;
                    t.send(Command::Intensity(level));
                }),
                options: ["Light", "Medium", "Heavy"]
                    .into_iter()
                    .map(|label| RadioItem {
                        label: label.into(),
                        ..Default::default()
                    })
                    .collect(),
            }
            .into(),
            CheckmarkItem {
                label: "Smooth (60 fps)".into(),
                checked: self.state.fps >= SMOOTH_FPS,
                activate: Box::new(|t: &mut Self| {
                    let fps = if t.state.fps >= SMOOTH_FPS {
                        DEFAULT_FPS
                    } else {
                        SMOOTH_FPS
                    };
                    t.state.fps = fps;
                    t.send(Command::Fps(fps));
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Tornado! (snow storm)".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Tornado)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Clear snow".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Clear)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| t.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<FlurryTray>,
}

impl Tray {
    /// Show the tray icon. Returns `None` (and logs a hint) if there is no
    /// tray host; everything still works through the CLI.
    pub fn spawn(tx: Sender<Request>, state: TrayState) -> Option<Tray> {
        let tray = FlurryTray {
            state,
            tx,
            icons: [22, 32, 48]
                .into_iter()
                .filter_map(snowflake_icon)
                .collect(),
        };
        match tray.spawn() {
            Ok(handle) => {
                log::debug!("tray icon registered");
                Some(Tray { handle })
            }
            Err(e) => {
                log::info!(
                    "no tray icon ({e}); use the CLI instead (`flurry toggle`, `flurry quit`). \
                     On GNOME, install the AppIndicator extension."
                );
                None
            }
        }
    }

    pub fn update(&self, state: TrayState) {
        self.handle.update(|t| t.state = state);
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.handle.shutdown().wait();
    }
}

/// A six-armed snowflake, `size`×`size`, as ARGB32 in network byte order.
fn snowflake_icon(size: u32) -> Option<Icon> {
    let mut pm = Pixmap::new(size, size)?;
    let s = size as f32;
    let (c, arm) = (s / 2.0, s * 0.42);
    let mut pb = PathBuilder::new();
    for k in 0..6 {
        let a = std::f32::consts::FRAC_PI_3 * k as f32 - std::f32::consts::FRAC_PI_2;
        let (dx, dy) = (a.cos(), a.sin());
        pb.move_to(c, c);
        pb.line_to(c + dx * arm, c + dy * arm);
        // Two small branches on each arm.
        let (bx, by) = (c + dx * arm * 0.6, c + dy * arm * 0.6);
        for side in [-1.0f32, 1.0] {
            let b = a + side * 0.8;
            pb.move_to(bx, by);
            pb.line_to(bx + b.cos() * arm * 0.35, by + b.sin() * arm * 0.35);
        }
    }
    let path = pb.finish()?;
    let width = (s / 11.0).max(1.5);
    let mut stroke = Stroke {
        width: width + 1.5,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    let mut paint = Paint::default();
    // A dark outline keeps it visible on light panels too.
    paint.set_color_rgba8(30, 50, 80, 200);
    pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    stroke.width = width;
    paint.set_color_rgba8(235, 245, 255, 255);
    pm.stroke_path(&path, &paint, &stroke, Transform::identity(), None);

    let data = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.alpha(), c.red(), c.green(), c.blue()]
        })
        .collect();
    Some(Icon {
        width: size as i32,
        height: size as i32,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_has_visible_pixels() {
        let icon = snowflake_icon(32).unwrap();
        assert_eq!(icon.data.len(), 32 * 32 * 4);
        let opaque = icon.data.chunks(4).filter(|p| p[0] > 128).count();
        assert!(opaque > 50, "only {opaque} opaque pixels");
    }
}
