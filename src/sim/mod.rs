//! Pure simulation: no platform code, fully unit-testable.
//! Must not import from `platform`, `tray` or `ipc` (PLAN.md §4).

pub mod flakes;
pub mod icicles;
pub mod particles;
pub mod piles;
pub mod snowballs;

use crate::config::{CarryModifier, Config};
use crate::geom::{AppWindow, Inputs, OutputInfo, Rect, WindowRect};
use flakes::{Flakes, Wind};
use particles::Particles;
use piles::{Piles, Z_PANEL};
use snowballs::{Env, Snowfolk};

/// Screen area that `flake_count` refers to. Bigger or more monitors get
/// proportionally more flakes so the snow looks equally dense everywhere.
pub const REFERENCE_AREA: f64 = 1920.0 * 1080.0;
const MAX_FLAKES: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimParams {
    /// Flakes per 1920×1080 of screen area.
    pub flake_count: u32,
    pub fall_speed: f32,
    pub wind: f32,
    pub max_pile_height: f32,
    pub melt_rate: f32,
    /// Snowballs and the cursor (`interaction` in the config).
    pub interaction: bool,
    pub carry_modifier: CarryModifier,
    /// Feature: busy windows melt their snow faster.
    pub hot_apps_melt: bool,
    /// Feature: icicles on snowy windows and panels.
    pub icicles: bool,
}

impl SimParams {
    pub fn from_config(c: &Config) -> Self {
        Self {
            flake_count: c.effective_flake_count(),
            fall_speed: c.fall_speed,
            wind: c.wind,
            max_pile_height: c.max_pile_height as f32,
            melt_rate: c.melt_rate,
            interaction: c.interaction,
            carry_modifier: c.carry_modifier,
            hot_apps_melt: c.features.hot_apps_melt,
            icicles: c.features.icicles,
        }
    }
}

impl Default for SimParams {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// The whole simulation state.
#[derive(Debug, Clone)]
pub struct World {
    params: SimParams,
    flakes: Flakes,
    wind: Wind,
    time: f64,
    bounds: Option<Rect>,
    screen_area: f64,
    piles: Piles,
    particles: Particles,
    snowfolk: Snowfolk,
    icicles: icicles::Icicles,
    /// The layout the piles were last synced to.
    layout: Layout,
}

/// Everything about the screen layout that affects piles.
#[derive(Debug, Clone, Default, PartialEq)]
struct Layout {
    outputs: Vec<Rect>,
    obstacles: Vec<WindowRect>,
    windows: Vec<AppWindow>,
    hidden: Vec<u64>,
}

impl Layout {
    fn of(inputs: &Inputs) -> Layout {
        Layout {
            outputs: inputs.outputs.iter().map(|o| o.rect).collect(),
            obstacles: inputs.obstacles.clone(),
            windows: inputs.windows.clone(),
            hidden: inputs.hidden_windows.clone(),
        }
    }
}

impl World {
    pub fn new(params: SimParams, seed: u64) -> Self {
        Self {
            params,
            flakes: Flakes::new(seed),
            // A different stream, so changing the flake count doesn't change the wind.
            wind: Wind::new(seed ^ 0x9E37_79B9_7F4A_7C15, params.wind),
            time: 0.0,
            bounds: None,
            screen_area: 0.0,
            piles: Piles::default(),
            particles: Particles::new(seed ^ 0x5DEE_CE66_D1CE_4E5B),
            snowfolk: Snowfolk::default(),
            icicles: icicles::Icicles::default(),
            layout: Layout::default(),
        }
    }

    /// Hanging and falling icicles.
    pub fn icicles(&self) -> Vec<icicles::Icicle> {
        if self.params.icicles {
            self.icicles.all()
        } else {
            Vec::new()
        }
    }

    /// Snowballs and snowmen.
    pub fn snowfolk(&self) -> &Snowfolk {
        &self.snowfolk
    }

    /// Global rects where clicks should go to flurry instead of the app
    /// below (to pick up snowballs). Empty unless the carry modifier is held
    /// or a ball is being carried.
    pub fn grab_rects(&self, cursor: Option<&crate::geom::CursorState>) -> Vec<Rect> {
        if !self.params.interaction {
            return Vec::new();
        }
        self.snowfolk.grab_rects(cursor, self.params.carry_modifier)
    }

    /// Snow in flight (knocked or spilled off windows).
    pub fn particles(&self) -> &Particles {
        &self.particles
    }

    /// Rects that hide piles below them, with their stacking depth (a pile
    /// is hidden by occluders with a greater depth): app windows by
    /// stacking index, panels on top.
    pub fn occluders(&self) -> impl Iterator<Item = (i32, Rect)> + '_ {
        let windows = self.layout.windows.iter().enumerate();
        windows
            .map(|(z, w)| (z as i32, w.rect))
            .chain(self.layout.obstacles.iter().map(|o| (Z_PANEL, o.rect)))
    }

    pub fn piles(&self) -> &Piles {
        &self.piles
    }

    pub fn params(&self) -> &SimParams {
        &self.params
    }

    pub fn flakes(&self) -> &Flakes {
        &self.flakes
    }

    pub fn wind(&self) -> &Wind {
        &self.wind
    }

    /// Seconds of simulated time.
    pub fn time(&self) -> f64 {
        self.time
    }

    /// Bounding box of all outputs, once known.
    pub fn bounds(&self) -> Option<Rect> {
        self.bounds
    }

    pub fn set_params(&mut self, params: SimParams) {
        self.params = params;
        self.wind.set_strength(params.wind);
        if let Some(b) = self.bounds {
            self.flakes.set_count(self.target_count(), &b);
        }
    }

    /// Remove all settled snow (piles, snowballs). Falling flakes keep falling.
    pub fn clear_settled(&mut self) {
        self.piles.clear();
        self.particles.clear();
        self.snowfolk.clear();
        self.icicles.clear();
    }

    /// Advance the simulation by `dt` seconds.
    pub fn step(&mut self, dt: f32, inputs: &Inputs) {
        let Some(bounds) = virtual_bounds(&inputs.outputs) else {
            return;
        };
        if self.bounds != Some(bounds) {
            // Outputs changed (startup or hotplug): spread snow over the new layout.
            self.bounds = Some(bounds);
            self.screen_area = inputs.outputs.iter().map(output_area).sum();
            self.flakes.scatter(self.target_count(), &bounds);
        }
        self.sync_piles(inputs);
        self.time += f64::from(dt);
        self.wind.update(self.time);
        let (piles, max_h) = (&mut self.piles, self.params.max_pile_height);
        self.flakes.step_landing(
            dt,
            self.time,
            self.wind.current(),
            self.params.fall_speed,
            &bounds,
            |f, y_before| f.layer.collides() && piles.try_land(f.x, y_before, f.y, f.r, max_h),
        );
        self.particles.step(dt, &bounds, |p, y_before| {
            piles.land(p.x, y_before, p.y, p.radius(), p.volume, max_h)
        });
        let no_heat = std::collections::HashMap::new();
        let heat = if self.params.hot_apps_melt {
            &inputs.window_heat
        } else {
            &no_heat
        };
        self.piles
            .step(dt, self.params.melt_rate, max_h, &mut self.particles, heat);
        let mut env = Env {
            dt,
            now: self.time,
            cursor: inputs.cursor.filter(|_| self.params.interaction),
            carry: self.params.carry_modifier,
            piles: &mut self.piles,
            particles: &mut self.particles,
            bounds,
            max_pile_height: max_h,
        };
        self.snowfolk.step(&mut env);
        if self.params.icicles {
            self.icicles
                .step(dt, inputs, &mut self.piles, &mut self.particles, bounds);
        }
    }

    fn sync_piles(&mut self, inputs: &Inputs) {
        let layout = Layout::of(inputs);
        if self.layout != layout {
            self.piles.sync(
                inputs,
                self.params.max_pile_height,
                self.params.melt_rate,
                self.time,
                &mut self.particles,
            );
            self.layout = layout;
        }
    }

    fn target_count(&self) -> usize {
        let n = f64::from(self.params.flake_count) * self.screen_area / REFERENCE_AREA;
        (n.round() as usize).min(MAX_FLAKES)
    }
}

fn output_area(o: &OutputInfo) -> f64 {
    f64::from(o.rect.w) * f64::from(o.rect.h)
}

/// Smallest rect containing every non-empty output.
pub fn virtual_bounds(outputs: &[OutputInfo]) -> Option<Rect> {
    let mut it = outputs.iter().map(|o| o.rect).filter(|r| !r.is_empty());
    let first = it.next()?;
    let (x0, y0, x1, y1) = it.fold(
        (first.x, first.y, first.right(), first.bottom()),
        |(x0, y0, x1, y1), r| {
            (
                x0.min(r.x),
                y0.min(r.y),
                x1.max(r.right()),
                y1.max(r.bottom()),
            )
        },
    );
    Some(Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    pub fn output(name: &str, rect: Rect) -> OutputInfo {
        OutputInfo {
            name: name.into(),
            rect,
            scale: 1.0,
        }
    }

    pub fn inputs_1080p() -> Inputs {
        Inputs {
            outputs: vec![output("TEST-1", Rect::new(0, 0, 1920, 1080))],
            ..Inputs::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;
    use std::time::{Duration, Instant};

    fn run(seed: u64, steps: u32) -> Vec<(f32, f32)> {
        let mut w = World::new(SimParams::default(), seed);
        let inputs = inputs_1080p();
        for _ in 0..steps {
            w.step(1.0 / 60.0, &inputs);
        }
        w.flakes().iter().map(|f| (f.x, f.y)).collect()
    }

    #[test]
    fn same_seed_same_output() {
        assert_eq!(run(42, 600), run(42, 600));
        assert_ne!(run(42, 600), run(43, 600));
    }

    #[test]
    fn no_outputs_means_no_flakes() {
        let mut w = World::new(SimParams::default(), 1);
        w.step(1.0 / 60.0, &Inputs::default());
        assert!(w.flakes().is_empty());
        assert_eq!(w.time(), 0.0);
    }

    #[test]
    fn flake_count_scales_with_screen_area() {
        let mut w = World::new(SimParams::default(), 1);
        w.step(1.0 / 60.0, &inputs_1080p());
        assert_eq!(w.flakes().len(), 300);

        let two = Inputs {
            outputs: vec![
                output("A", Rect::new(0, 0, 1920, 1080)),
                output("B", Rect::new(1920, -200, 1920, 1080)),
            ],
            ..Inputs::default()
        };
        w.step(1.0 / 60.0, &two);
        assert_eq!(w.flakes().len(), 600);
        assert_eq!(w.bounds(), Some(Rect::new(0, -200, 3840, 1280)));
    }

    #[test]
    fn set_params_changes_count() {
        let mut w = World::new(SimParams::default(), 1);
        w.step(1.0 / 60.0, &inputs_1080p());
        w.set_params(SimParams {
            flake_count: 600,
            ..SimParams::default()
        });
        assert_eq!(w.flakes().len(), 600);
    }

    #[test]
    fn virtual_bounds_ignores_empty_outputs() {
        let outs = [
            output("A", Rect::new(10, 10, 0, 0)),
            output("B", Rect::new(100, 50, 200, 100)),
        ];
        assert_eq!(virtual_bounds(&outs), Some(Rect::new(100, 50, 200, 100)));
        assert_eq!(virtual_bounds(&[]), None);
    }

    #[test]
    fn snow_builds_up_on_the_panel_and_stays_below_max() {
        let mut w = World::new(SimParams::default(), 3);
        let mut inputs = inputs_1080p();
        inputs.obstacles.push(WindowRect {
            id: 1,
            rect: Rect::new(0, 1040, 1920, 40),
        });
        let max = w.params().max_pile_height;
        let mut samples = Vec::new();
        for minute in 0..10 {
            for _ in 0..60 * 60 {
                w.step(1.0 / 60.0, &inputs);
            }
            let s = &w.piles().surfaces()[0];
            let mean = s.volume() / (s.heights.len() as f32 * piles::COL_W);
            samples.push((minute + 1, mean, s.max_height()));
            assert!(s.max_height() <= max);
        }
        eprintln!("(minute, mean height, max height): {samples:?}");
        let (_, mean_10, _) = samples[9];
        assert!(
            (4.0..=max * 0.8).contains(&mean_10),
            "after 10 min mean height {mean_10}"
        );
        w.clear_settled();
        assert_eq!(w.piles().volume(), 0.0);
    }

    /// PLAN.md Phase 1: one step with 300 flakes takes < 0.2 ms.
    #[test]
    fn bench_step_300_flakes() {
        let mut w = World::new(SimParams::default(), 7);
        let inputs = inputs_1080p();
        w.step(1.0 / 60.0, &inputs);
        assert_eq!(w.flakes().len(), 300);
        let best = (0..20)
            .map(|_| {
                let t = Instant::now();
                for _ in 0..100 {
                    w.step(1.0 / 60.0, &inputs);
                }
                t.elapsed() / 100
            })
            .min()
            .unwrap_or(Duration::MAX);
        eprintln!("sim step, 300 flakes: {best:?}");
        assert!(best < Duration::from_micros(200), "step took {best:?}");
    }
}
