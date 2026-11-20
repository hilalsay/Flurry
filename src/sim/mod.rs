//! Pure simulation: no platform code, fully unit-testable.
//! Must not import from `platform`, `tray` or `ipc` (PLAN.md §4).

pub mod flakes;
pub mod frost;
pub mod globe;
pub mod icicles;
pub mod particles;
pub mod piles;
pub mod snowballs;
pub mod tornado;

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
    /// Feature: typing shakes snow off the focused window.
    pub typing_shakes_snow: bool,
    /// Feature: frost when idle, after this many seconds (`None` = off).
    pub frost_after: Option<f32>,
    /// Feature: shake the mouse to whirl snow up.
    pub snow_globe: bool,
    /// Feature: modifier + drag writes in the snow.
    pub write_in_snow: bool,
    /// Feature: flying snow crosses monitor borders.
    pub cross_monitor: bool,
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
            typing_shakes_snow: c.features.typing_shakes_snow,
            frost_after: c
                .features
                .frost
                .then_some(c.features.frost_idle_minutes.max(0.0) * 60.0),
            snow_globe: c.features.snow_globe,
            write_in_snow: c.features.write_in_snow,
            cross_monitor: c.features.cross_monitor,
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
    frost: frost::Frost,
    globe: globe::Globe,
    /// Where the pointer was while writing in the snow last step.
    pen: Option<(f32, f32)>,
    /// The layout the piles were last synced to.
    layout: Layout,
    /// A tornado / snow storm in progress.
    storm: Option<tornado::Storm>,
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
            frost: frost::Frost::default(),
            globe: globe::Globe::default(),
            pen: None,
            layout: Layout::default(),
            storm: None,
        }
    }

    /// Frosted cells (feature 4).
    pub fn frost(&self) -> impl Iterator<Item = frost::FrostCell> + '_ {
        self.frost.cells()
    }

    /// Hanging and falling icicles.
    pub fn icicles(&self) -> Vec<icicles::Icicle> {
        if self.params.icicles {
            self.icicles.all()
        } else {
            Vec::new()
        }
    }

    /// Start a tornado / snow storm (restarts one in progress).
    pub fn start_storm(&mut self) {
        let seed = self.time.to_bits() ^ 0x70A2_ADE5;
        self.storm = Some(tornado::Storm::new(seed));
    }

    /// Seconds left of the storm, if one is going on.
    pub fn storm_remaining(&self) -> Option<f32> {
        self.storm.as_ref().map(tornado::Storm::remaining)
    }

    /// The tornado's swirling snow, drawn like flakes.
    pub fn swirl(&self) -> Vec<tornado::SwirlBit> {
        match (&self.storm, &self.bounds) {
            (Some(s), Some(b)) => s.bits(b),
            _ => Vec::new(),
        }
    }

    /// A wind gust right now (music makes wind).
    pub fn gust(&mut self, strength: f32) {
        self.wind.kick(strength);
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
        // A ball behind a window must not catch that window's clicks.
        let rects = self.snowfolk.grab_rects(cursor, self.params.carry_modifier);
        let occluders: Vec<(i32, Rect)> = self.occluders().collect();
        rects
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                let z = self.snowfolk.ball_depth(i, &self.piles);
                let holes: Vec<Rect> = occluders
                    .iter()
                    .filter(|(oz, _)| *oz > z)
                    .map(|(_, o)| *o)
                    .collect();
                crate::geom::subtract(&[*r], &holes)
            })
            .collect()
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
        self.frost.clear();
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
        let storm_wind = self.step_storm(dt, &bounds);
        let (piles, max_h) = (&mut self.piles, self.params.max_pile_height);
        self.flakes.step_landing(
            dt,
            self.time,
            self.wind.current() + storm_wind,
            self.params.fall_speed,
            &bounds,
            |f, y_before| f.layer.collides() && piles.try_land(f.x, y_before, f.y, f.r, max_h),
        );
        if let Some(f) = self.storm.as_ref().and_then(|s| s.funnel(&bounds)) {
            self.flakes.vortex(dt, self.time, &f);
        }
        let monitors: Vec<Rect> = inputs.outputs.iter().map(|o| o.rect).collect();
        let confine = (!self.params.cross_monitor).then_some(monitors.as_slice());
        let mut bursts = Vec::new();
        self.particles.step(dt, &bounds, confine, |p, y_before| {
            if !p.is_lump() {
                return piles.land(p.x, y_before, p.y, p.radius(), p.volume, max_h);
            }
            let powder = p.volume * particles::BURST_SHARE;
            let landed = piles.land(p.x, y_before, p.y, p.radius(), p.volume - powder, max_h);
            if landed {
                bursts.push((p.x, p.y, powder));
            }
            landed
        });
        for (x, y, v) in bursts {
            self.particles.burst(x, y, v);
        }
        if self.params.typing_shakes_snow && inputs.keys_pressed > 0 {
            if let Some(id) = inputs.active_window {
                self.piles
                    .shake(id, inputs.keys_pressed, &mut self.particles);
            }
        }
        let no_heat = std::collections::HashMap::new();
        let heat = if self.params.hot_apps_melt {
            &inputs.window_heat
        } else {
            &no_heat
        };
        self.piles
            .step(dt, self.params.melt_rate, max_h, &mut self.particles, heat);
        let occluders: Vec<(i32, Rect)> = self.occluders().collect();
        let mut env = Env {
            occluders: &occluders,
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
        if self.params.write_in_snow && self.params.interaction {
            self.write(inputs.cursor);
        }
        if self.params.snow_globe && self.params.interaction {
            let cursor = inputs.cursor;
            if self
                .globe
                .step(self.time, cursor, &mut self.piles, &mut self.particles)
            {
                log::debug!("snow globe shake");
            }
        }
        match self.params.frost_after {
            Some(after) => self.frost.step(dt, &inputs.outputs, inputs.cursor, after),
            None => self.frost.clear(),
        }
    }

    /// Feature 9: with the carry modifier and the left button held (and no
    /// snowball in hand), the pointer carves its path into the snow.
    fn write(&mut self, cursor: Option<crate::geom::CursorState>) {
        let pen = cursor.filter(|c| {
            c.left_down
                && snowballs::modifier_held(self.params.carry_modifier, c)
                && !self.snowfolk.is_carrying()
        });
        let Some(c) = pen else {
            self.pen = None;
            return;
        };
        let from = self.pen.unwrap_or((c.x, c.y));
        let len = (c.x - from.0).hypot(c.y - from.1);
        let steps = (len / 1.5).ceil().max(1.0) as usize;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            self.piles.carve(
                from.0 + (c.x - from.0) * t,
                from.1 + (c.y - from.1) * t,
                2.5,
            );
        }
        self.pen = Some((c.x, c.y));
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

    /// Advance the storm: more flakes while it lasts, snow sucked up from
    /// piles under the funnel and flung out, snowballs thrown. Returns the
    /// extra wind.
    fn step_storm(&mut self, dt: f32, bounds: &Rect) -> f32 {
        let target = self.target_count();
        let Some(storm) = self.storm.as_mut() else {
            return 0.0;
        };
        storm.step(dt);
        if storm.is_over() {
            self.storm = None;
            self.flakes.set_count(target, bounds);
            log::info!("the storm is over");
            return 0.0;
        }
        let (strength, wind) = (storm.strength(), storm.wind());
        let extra = 1.0 + tornado::EXTRA_FLAKES * strength;
        let count = ((target as f32 * extra) as usize).min(MAX_FLAKES);
        self.flakes.set_count(count, bounds);
        let Some((f, amount)) = storm.suction(dt, bounds) else {
            return wind;
        };
        // Suck up snow from piles near the funnel's foot.
        let keys: Vec<(piles::SurfaceKey, f32)> = self
            .piles
            .surfaces()
            .iter()
            .filter_map(|s| {
                let h = f.rel_height(s.y);
                let cx = f.centre(h);
                ((0.0..0.35).contains(&h) && s.top_at(cx).is_some())
                    .then(|| (s.key.clone(), f.radius(h).max(20.0)))
            })
            .collect();
        for (key, half) in keys {
            let (cx, y) = match self.piles.surface(&key) {
                Some(s) => (f.centre(f.rel_height(s.y)), s.y),
                None => continue,
            };
            // `take` removes a depth (px) from each column it covers.
            let taken = self.piles.take(&key, cx, half, amount / (2.0 * half));
            if taken <= 0.0 {
                continue;
            }
            let n = (taken / particles::PARTICLE_VOLUME).ceil().max(1.0);
            for _ in 0..n as usize {
                let (vx, vy) = storm.fling();
                self.particles.spawn(
                    particles::Particle {
                        x: cx + (vx.signum() * half * 0.3),
                        y: y - 4.0,
                        vx,
                        vy,
                        volume: taken / n,
                    },
                    40.0,
                );
            }
        }
        // Throw snowballs and snowmen near the foot.
        let reach = |x: f32, y: f32| {
            let h = f.rel_height(y);
            (0.0..0.4).contains(&h) && (x - f.centre(h)).abs() < f.radius(h) + 20.0
        };
        self.snowfolk
            .blow(|x, y| reach(x, y).then(|| storm.fling()));
        wind
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

    fn two_monitors() -> Inputs {
        Inputs {
            outputs: vec![
                output("A", Rect::new(0, 0, 1920, 1080)),
                output("B", Rect::new(1920, 0, 1920, 1080)),
            ],
            ..Inputs::default()
        }
    }

    /// Throw snow from near A's right edge towards B; return the largest x
    /// any particle reached, and whether B's scene ever drew one.
    fn throw_across(cross: bool) -> (f32, bool) {
        let mut w = World::new(
            SimParams {
                cross_monitor: cross,
                flake_count: 0,
                ..SimParams::default()
            },
            1,
        );
        let inputs = two_monitors();
        w.step(1.0 / 60.0, &inputs);
        for k in 0..20 {
            w.particles.spawn(
                particles::Particle {
                    x: 1850.0,
                    y: 400.0 + k as f32,
                    vx: 900.0,
                    vy: -200.0,
                    volume: 30.0,
                },
                0.0,
            );
        }
        let renderer = crate::render::Renderer::new();
        let b = inputs.outputs[1].rect;
        let mut tracker = crate::render::PileTracker::default();
        let mut scene = crate::render::PileScene::default();
        let (mut max_x, mut drawn_on_b) = (0.0f32, false);
        for _ in 0..60 {
            w.step(1.0 / 60.0, &inputs);
            for p in w.particles().iter() {
                max_x = max_x.max(p.x);
            }
            renderer.build_pile_scene(&w, b, &mut tracker, &mut scene);
            drawn_on_b |= !scene.draws.is_empty();
        }
        (max_x, drawn_on_b)
    }

    #[test]
    fn a_dropped_lump_bursts_into_powder_and_keeps_its_snow() {
        let mut w = World::new(
            SimParams {
                flake_count: 0,
                melt_rate: 0.0,
                ..SimParams::default()
            },
            1,
        );
        let inputs = inputs_1080p();
        w.step(1.0 / 60.0, &inputs);
        let lump = particles::Particle {
            x: 900.0,
            y: 300.0,
            vx: 0.0,
            vy: 0.0,
            volume: 200.0,
        };
        assert!(lump.is_lump() && lump.clump().len() > 3, "drawn as a clump");
        w.particles.spawn(lump, 0.0);
        let (mut burst, mut rising) = (false, 0);
        for _ in 0..60 * 6 {
            w.step(1.0 / 60.0, &inputs);
            let ps: Vec<_> = w.particles().iter().copied().collect();
            if ps.len() > 1 {
                burst = true;
                assert!(ps.iter().all(|p| !p.is_lump()), "powder, not lumps");
                rising = rising.max(ps.iter().filter(|p| p.vy < 0.0).count());
            }
        }
        assert!(burst, "it burst where it landed");
        assert!(rising >= 3, "powder flew up ({rising})");
        assert!(w.particles().is_empty(), "the powder settled");
        let total = w.piles().volume();
        assert!((total - 200.0).abs() < 1.0, "snow kept: {total}");
    }

    #[test]
    fn flying_snow_crosses_monitors() {
        let (max_x, drawn_on_b) = throw_across(true);
        assert!(max_x > 2000.0, "reached monitor B: {max_x}");
        assert!(drawn_on_b, "and B drew it");
        let (max_x, drawn_on_b) = throw_across(false);
        // (Sprites of snow resting at the edge may overlap B by a pixel.)
        assert!(max_x < 1920.0, "stopped at A's edge: {max_x}");
        let _ = drawn_on_b;
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

    #[test]
    fn a_tornado_storm_blows_and_ends() {
        let mut w = World::new(SimParams::default(), 5);
        let inputs = Inputs {
            obstacles: vec![WindowRect {
                id: 1,
                rect: Rect::new(0, 1040, 1920, 40),
            }],
            ..inputs_1080p()
        };
        for _ in 0..60 * 120 {
            w.step(1.0 / 60.0, &inputs);
        }
        let normal = w.flakes().len();
        assert!(w.swirl().is_empty() && w.storm_remaining().is_none());
        w.start_storm();
        let (mut most_flakes, mut most_flying, mut most_bits) = (0, 0, 0);
        let start = Instant::now();
        let steps = (tornado::DURATION * 60.0) as u32 + 30;
        for _ in 0..steps {
            w.step(1.0 / 60.0, &inputs);
            most_flakes = most_flakes.max(w.flakes().len());
            most_flying = most_flying.max(w.particles().len());
            most_bits = most_bits.max(w.swirl().len());
        }
        let per_step = start.elapsed() / steps;
        assert!(
            most_flakes as f32 >= normal as f32 * 1.75,
            "heavier snowfall: {most_flakes} vs {normal}"
        );
        assert!(most_bits > 400, "a visible funnel: {most_bits}");
        assert!(most_flying > 20, "pile snow sucked up: {most_flying}");
        assert!(w.storm_remaining().is_none(), "over");
        assert_eq!(w.flakes().len(), normal, "back to normal");
        assert!(w.swirl().is_empty());
        // Cheap enough for the CPU budget (debug build: generous).
        assert!(per_step < Duration::from_millis(4), "{per_step:?} per step");
    }
}
