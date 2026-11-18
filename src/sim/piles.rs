//! Settled snow: one heightmap per surface (screen bottom, panel tops,
//! window tops). Columns are `COL_W` px wide; heights in px. Snow lands
//! from flakes and particles, slides downhill when too steep, creeps,
//! melts, and falls off window edges. Window piles ride along with slow
//! moves and are knocked off by fast ones.

use std::collections::HashMap;

use crate::geom::{Inputs, Rect};
use crate::sim::particles::{Particle, Particles, PARTICLE_VOLUME};

/// Column width in px.
pub const COL_W: f32 = 2.0;
/// Neighbouring columns may differ by this much (px) before snow slides.
const MAX_STEP: f32 = 1.0;
/// Fraction of the excess moved per step (relaxation keeps it smooth).
const SLUMP_RATE: f32 = 0.5;
/// Snow creep per step: smooths small bumps (diffusion; must stay < 0.5).
const CREEP: f32 = 0.06;
/// A landed flake spreads its snow over ± this many radii.
const SPREAD: f32 = 2.5;
/// Snow volume added per landed flake: π r² × this (px²).
const FLAKE_GAIN: f32 = 1.2;
/// Melting at `melt_rate = 1`: a constant part plus a part proportional
/// to height, so tall piles shrink faster (px/s).
const MELT_BASE: f32 = 0.012;
const MELT_PER_PX: f32 = 0.0035;
/// A window moving faster than this (px/s) throws its snow off.
const KNOCK_SPEED: f32 = 300.0;
/// Thrown snow keeps this share of the window's velocity.
const KNOCK_CARRY: f32 = 0.9;
/// A window using one full core melts its snow this many times faster.
const HEAT_MELT: f32 = 25.0;
/// Below this load a window counts as cold.
const HEAT_MIN: f32 = 0.15;
/// Drips per second from a pile on a window using one full core.
const DRIPS_PER_LOAD: f32 = 6.0;

/// Stacking depth of a surface: ground below every window, panels above.
pub const Z_GROUND: i32 = -1;
pub const Z_PANEL: i32 = i32::MAX;

/// Identifies a surface across frames, so its snow survives layout changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SurfaceKey {
    /// A stretch of a monitor's bottom edge not covered by a panel.
    Ground { output: String, segment: u32 },
    /// The top edge of a panel/dock.
    Obstacle(u64),
    /// The top edge of an app window.
    Window(u64),
}

impl SurfaceKey {
    fn spills(&self) -> bool {
        matches!(self, SurfaceKey::Window(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub key: SurfaceKey,
    /// Left edge, global px.
    pub x0: f32,
    /// The line snow rests on (panel/window top, screen bottom), global px.
    pub y: f32,
    /// Stacking depth: `Z_GROUND`, the window's stacking index, or `Z_PANEL`.
    pub z: i32,
    /// Snow height per column, px.
    pub heights: Vec<f32>,
    /// Sim time of the last move, for the window's velocity.
    moved_at: f64,
    /// Snow that slid over the left/right edge, not yet a particle (px²).
    spill: [f32; 2],
}

impl Surface {
    fn new(spec: &Spec, now: f64) -> Surface {
        Surface {
            key: spec.key.clone(),
            x0: spec.x0,
            y: spec.y,
            z: spec.z,
            heights: vec![0.0; columns(spec.x0, spec.x1)],
            moved_at: now,
            spill: [0.0; 2],
        }
    }

    /// Right edge, global px.
    pub fn x1(&self) -> f32 {
        self.x0 + self.heights.len() as f32 * COL_W
    }

    /// Total snow (px²).
    pub fn volume(&self) -> f32 {
        self.heights.iter().sum::<f32>() * COL_W
    }

    pub fn max_height(&self) -> f32 {
        self.heights.iter().copied().fold(0.0, f32::max)
    }

    /// Top of the snow at global `x`, if `x` is over this surface.
    pub fn top_at(&self, x: f32) -> Option<f32> {
        self.col(x).map(|c| self.y - self.heights[c])
    }

    fn col(&self, x: f32) -> Option<usize> {
        let c = ((x - self.x0) / COL_W).floor();
        (c >= 0.0 && (c as usize) < self.heights.len()).then_some(c as usize)
    }

    /// Move onto a new span, keeping the snow at the same global x
    /// (panels and ground: the snow stays put while the edge changes).
    fn refit_in_place(&mut self, spec: &Spec) {
        let old = std::mem::take(&mut self.heights);
        let old_x0 = self.x0;
        self.heights = (0..columns(spec.x0, spec.x1))
            .map(|c| {
                let x = spec.x0 + c as f32 * COL_W;
                let i = ((x - old_x0) / COL_W).round();
                if i >= 0.0 {
                    old.get(i as usize).copied().unwrap_or(0.0)
                } else {
                    0.0
                }
            })
            .collect();
        (self.x0, self.y, self.z) = (spec.x0, spec.y, spec.z);
    }

    /// Move with the window, stretching the snow if the width changed.
    fn refit_with_window(&mut self, spec: &Spec) {
        let n = columns(spec.x0, spec.x1);
        if n != self.heights.len() {
            let old = std::mem::take(&mut self.heights);
            let scale = old.len() as f32 / n as f32;
            self.heights = (0..n)
                .map(|c| old[((c as f32 + 0.5) * scale) as usize % old.len().max(1)])
                .collect();
        }
        (self.x0, self.y, self.z) = (spec.x0, spec.y, spec.z);
    }

    /// Break all snow into particles moving at `v` (px/s), and clear it.
    fn shatter(&mut self, v: (f32, f32), particles: &mut Particles) {
        let mut acc = 0.0;
        for (i, h) in self.heights.iter_mut().enumerate() {
            acc += *h * COL_W;
            if acc >= PARTICLE_VOLUME {
                particles.spawn(
                    Particle {
                        x: self.x0 + (i as f32 + 0.5) * COL_W,
                        y: self.y - *h * 0.5,
                        vx: v.0,
                        vy: v.1,
                        volume: acc,
                    },
                    25.0 + 0.08 * (v.0.abs() + v.1.abs()),
                );
                acc = 0.0;
            }
            *h = 0.0;
        }
    }

    /// Let snow slide between neighbours steeper than `MAX_STEP`, and creep
    /// a little everywhere. Panel and ground ends are walls; window ends
    /// spill (collected in `self.spill`). Inside, the volume is unchanged.
    fn slump(&mut self) {
        let spills = self.key.spills();
        let h = &mut self.heights;
        let n = h.len();
        if n >= 3 {
            // Flux between neighbours, computed first so the update is symmetric.
            let mut prev_flux = 0.0;
            for i in 0..n - 1 {
                let flux = CREEP * 0.5 * (h[i] - h[i + 1]);
                h[i] -= flux - prev_flux;
                prev_flux = flux;
            }
            h[n - 1] += prev_flux;
        }
        for i in 0..n.saturating_sub(1) {
            let d = h[i] - h[i + 1];
            if d.abs() > MAX_STEP {
                let m = (d.abs() - MAX_STEP) * 0.5 * SLUMP_RATE * d.signum();
                h[i] -= m;
                h[i + 1] += m;
            }
        }
        if spills && n > 0 {
            // Outside the window there is nothing: an edge column taller than
            // `MAX_STEP` sheds its excess.
            for (side, i) in [(0, 0), (1, n - 1)] {
                if h[i] > MAX_STEP {
                    let m = (h[i] - MAX_STEP) * 0.5 * SLUMP_RATE;
                    h[i] -= m;
                    self.spill[side] += m * COL_W;
                }
            }
        }
    }

    /// Turn spilled snow into particles dropping off the edges.
    fn emit_spill(&mut self, particles: &mut Particles) {
        let edges = [self.x0 - 1.0, self.x1() + 1.0];
        for side in 0..2 {
            if self.spill[side] >= PARTICLE_VOLUME {
                let i = if side == 0 { 0 } else { self.heights.len() - 1 };
                particles.spawn(
                    Particle {
                        x: edges[side],
                        y: self.y - self.heights[i] - 1.0,
                        vx: if side == 0 { -20.0 } else { 20.0 },
                        vy: 0.0,
                        volume: self.spill[side],
                    },
                    8.0,
                );
                self.spill[side] = 0.0;
            }
        }
    }

    fn melt(&mut self, dt: f32, rate: f32, max_height: f32) {
        for h in &mut self.heights {
            *h = (*h - dt * rate * (MELT_BASE + MELT_PER_PX * *h)).clamp(0.0, max_height);
        }
    }
}

fn columns(x0: f32, x1: f32) -> usize {
    (((x1 - x0) / COL_W).ceil() as usize).max(1)
}

/// Where a surface should be this frame.
#[derive(Debug, Clone, PartialEq)]
struct Spec {
    key: SurfaceKey,
    x0: f32,
    x1: f32,
    y: f32,
    z: i32,
}

/// A window pile set aside while its window is hidden.
#[derive(Debug, Clone)]
struct Parked {
    surface: Surface,
    since: f64,
}

/// All piles.
#[derive(Debug, Clone)]
pub struct Piles {
    surfaces: Vec<Surface>,
    parked: HashMap<u64, Parked>,
    /// Window piles thrown off in the last `sync`, with the window's velocity.
    knocked: Vec<(SurfaceKey, (f32, f32))>,
    rng: fastrand::Rng,
}

impl Default for Piles {
    fn default() -> Self {
        Self {
            surfaces: Vec::new(),
            parked: HashMap::new(),
            knocked: Vec::new(),
            rng: fastrand::Rng::with_seed(0x51EE7),
        }
    }
}

impl Piles {
    pub fn surfaces(&self) -> &[Surface] {
        &self.surfaces
    }

    pub fn surface(&self, key: &SurfaceKey) -> Option<&Surface> {
        self.surfaces.iter().find(|s| &s.key == key)
    }

    /// Windows that moved fast enough to throw their snow in the last sync.
    pub fn knocked(&self) -> &[(SurfaceKey, (f32, f32))] {
        &self.knocked
    }

    /// The first surface something at `x` meets falling from `prev_bottom`
    /// to `bottom` (its lowest point), and the snow top there.
    pub fn landing(&self, x: f32, prev_bottom: f32, bottom: f32) -> Option<(SurfaceKey, f32)> {
        self.hit(x, prev_bottom, bottom)
            .map(|(i, top)| (self.surfaces[i].key.clone(), top))
    }

    fn hit(&self, x: f32, prev_bottom: f32, bottom: f32) -> Option<(usize, f32)> {
        let mut hit: Option<(usize, f32)> = None;
        for (i, s) in self.surfaces.iter().enumerate() {
            let Some(top) = s.top_at(x) else { continue };
            if prev_bottom <= top + 0.5 && bottom >= top && hit.is_none_or(|(_, t)| top < t) {
                hit = Some((i, top));
            }
        }
        hit
    }

    /// The surface whose snow `(x, y)` is in (or just above), if the snow
    /// there is at least `min_depth` px deep.
    pub fn snow_at(&self, x: f32, y: f32, min_depth: f32) -> Option<(SurfaceKey, f32)> {
        self.surfaces.iter().find_map(|s| {
            let c = s.col(x)?;
            let top = s.y - s.heights[c];
            (s.heights[c] >= min_depth && y >= top - 3.0 && y <= s.y + 2.0)
                .then(|| (s.key.clone(), top))
        })
    }

    /// Remove up to `depth` px of snow from the columns of `key` within
    /// `x ± half_width`. Returns the volume taken (px²).
    pub fn take(&mut self, key: &SurfaceKey, x: f32, half_width: f32, depth: f32) -> f32 {
        let Some(s) = self.surfaces.iter_mut().find(|s| &s.key == key) else {
            return 0.0;
        };
        let (Some(c0), Some(c1)) = (
            s.col((x - half_width).max(s.x0)),
            s.col((x + half_width).min(s.x1() - 0.01)),
        ) else {
            return 0.0;
        };
        let mut taken = 0.0;
        for h in &mut s.heights[c0..=c1] {
            let t = h.min(depth);
            *h -= t;
            taken += t * COL_W;
        }
        taken
    }

    /// Total snow on all visible surfaces (px²).
    pub fn volume(&self) -> f32 {
        self.surfaces.iter().map(Surface::volume).sum()
    }

    pub fn clear(&mut self) {
        for s in &mut self.surfaces {
            s.heights.fill(0.0);
            s.spill = [0.0; 2];
        }
        self.parked.clear();
    }

    /// Match the surfaces to the current monitors, panels and windows at sim
    /// time `now`, keeping snow where surfaces persist:
    /// - panels/ground keep snow at the same place while their edge changes;
    /// - window piles ride along with slow moves, are thrown off by fast
    ///   ones, and are stretched when the window is resized;
    /// - a window that is hidden (minimized, other workspace) has its pile
    ///   parked and restored (minus melting) when it comes back;
    /// - a window that closed or stopped collecting (maximized) drops its
    ///   snow as particles.
    pub fn sync(
        &mut self,
        inputs: &Inputs,
        max_height: f32,
        melt_rate: f32,
        now: f64,
        particles: &mut Particles,
    ) {
        let mut old: HashMap<SurfaceKey, Surface> = self
            .surfaces
            .drain(..)
            .map(|s| (s.key.clone(), s))
            .collect();
        self.knocked.clear();
        for spec in specs(inputs, max_height) {
            let surface = match old.remove(&spec.key) {
                Some(s) => follow(s, &spec, now, particles, &mut self.knocked),
                None => self.unpark(&spec, now, max_height, melt_rate),
            };
            self.surfaces.push(surface);
        }
        for (key, mut s) in old {
            match key {
                SurfaceKey::Window(id) if inputs.hidden_windows.contains(&id) => {
                    self.parked.insert(
                        id,
                        Parked {
                            surface: s,
                            since: now,
                        },
                    );
                }
                SurfaceKey::Window(_) => s.shatter((0.0, 0.0), particles),
                _ => {} // a panel/ground stretch vanished: its snow goes with it
            }
        }
        // Forget parked piles of windows that no longer exist at all.
        let alive = |id: &u64| inputs.hidden_windows.contains(id);
        self.parked.retain(|id, _| alive(id));
    }

    fn unpark(&mut self, spec: &Spec, now: f64, max_height: f32, melt_rate: f32) -> Surface {
        let SurfaceKey::Window(id) = spec.key else {
            return Surface::new(spec, now);
        };
        let Some(p) = self.parked.remove(&id) else {
            return Surface::new(spec, now);
        };
        let mut s = p.surface;
        // It kept melting while hidden.
        s.melt((now - p.since) as f32, melt_rate, max_height);
        s.refit_with_window(spec);
        s.moved_at = now;
        s
    }

    /// If a flake of radius `r` at `x` moving from `y_prev` to `y` (centre)
    /// reaches a pile, add its snow there and return true.
    pub fn try_land(&mut self, x: f32, y_prev: f32, y: f32, r: f32, max_height: f32) -> bool {
        let volume = std::f32::consts::PI * r * r * FLAKE_GAIN;
        self.land(x, y_prev, y, r, volume, max_height)
    }

    /// Like [`try_land`](Self::try_land) with an explicit snow `volume` (px²).
    pub fn land(&mut self, x: f32, y_prev: f32, y: f32, r: f32, volume: f32, max_h: f32) -> bool {
        // The first surface met on the way down.
        let Some((i, _)) = self.hit(x, y_prev + r, y + r) else {
            return false;
        };
        let s = &mut self.surfaces[i];
        let reach = r * SPREAD;
        let (Some(c0), Some(c1)) = (
            s.col((x - reach).max(s.x0)),
            s.col((x + reach).min(s.x1() - 0.01)),
        ) else {
            return false;
        };
        // Triangle-shaped deposit centred on the landing point.
        let (x0, ramp) = (s.x0, reach + COL_W);
        let weight = |c: usize| {
            let cx = x0 + (c as f32 + 0.5) * COL_W;
            (1.0 - (cx - x).abs() / ramp).max(0.0)
        };
        let total: f32 = (c0..=c1).map(weight).sum::<f32>().max(1e-3);
        let height = volume / COL_W;
        for (c, h) in s.heights.iter_mut().enumerate().take(c1 + 1).skip(c0) {
            *h = (*h + height * weight(c) / total).min(max_h);
        }
        true
    }

    /// Slide, spill and melt. `heat` (window id → CPU load) makes busy
    /// windows melt faster and drip.
    pub fn step(
        &mut self,
        dt: f32,
        melt_rate: f32,
        max_height: f32,
        particles: &mut Particles,
        heat: &HashMap<u64, f32>,
    ) {
        for s in &mut self.surfaces {
            s.slump();
            s.emit_spill(particles);
            let load = match s.key {
                SurfaceKey::Window(id) => heat.get(&id).copied().unwrap_or(0.0),
                _ => 0.0,
            };
            if load < HEAT_MIN {
                s.melt(dt, melt_rate, max_height);
                continue;
            }
            s.melt(dt, melt_rate * (1.0 + HEAT_MELT * load), max_height);
            // Meltwater drips off the front edge.
            if self.rng.f32() < DRIPS_PER_LOAD * load.min(4.0) * dt {
                let c = self.rng.usize(..s.heights.len());
                if s.heights[c] > 0.5 {
                    particles.spawn(
                        Particle {
                            x: s.x0 + (c as f32 + 0.5) * COL_W,
                            y: s.y + 2.0,
                            vx: 0.0,
                            vy: 30.0,
                            volume: 2.0,
                        },
                        2.0,
                    );
                }
            }
        }
    }
}

/// Follow a surface that still exists to its new place (see `Piles::sync`).
fn follow(
    mut s: Surface,
    spec: &Spec,
    now: f64,
    particles: &mut Particles,
    knocked: &mut Vec<(SurfaceKey, (f32, f32))>,
) -> Surface {
    if (s.x0, s.x1(), s.y, s.z) == (spec.x0, spec.x1, spec.y, spec.z) {
        return s;
    }
    if !s.key.spills() {
        s.refit_in_place(spec);
        return s;
    }
    let moved = (spec.x0 - s.x0, spec.y - s.y);
    if moved != (0.0, 0.0) {
        let dt = ((now - s.moved_at) as f32).max(1.0 / 60.0);
        let v = (moved.0 / dt, moved.1 / dt);
        if v.0.hypot(v.1) > KNOCK_SPEED {
            let thrown = (v.0 * KNOCK_CARRY, v.1 * KNOCK_CARRY);
            s.shatter(thrown, particles);
            knocked.push((s.key.clone(), thrown));
        }
        s.moved_at = now;
    }
    s.refit_with_window(spec);
    s
}

/// Surfaces for the current layout:
/// - each panel's top edge, if it has room above it on its monitor;
/// - each collecting window's top edge, likewise;
/// - each monitor's bottom edge, minus the stretches covered by panels
///   that sit on that edge.
fn specs(inputs: &Inputs, max_height: f32) -> Vec<Spec> {
    let room_above = |r: &Rect| {
        let cx = r.x + r.w as i32 / 2;
        inputs
            .outputs
            .iter()
            .any(|out| out.rect.contains(cx, r.y) && r.y as f32 > out.rect.y as f32 + max_height)
    };
    let mut specs = Vec::new();
    for o in &inputs.obstacles {
        if o.rect.w > 0 && room_above(&o.rect) {
            specs.push(Spec {
                key: SurfaceKey::Obstacle(o.id),
                x0: o.rect.x as f32,
                x1: o.rect.right() as f32,
                y: o.rect.y as f32,
                z: Z_PANEL,
            });
        }
    }
    for (z, w) in inputs.windows.iter().enumerate() {
        if w.collects && w.rect.w >= 8 && room_above(&w.rect) {
            specs.push(Spec {
                key: SurfaceKey::Window(w.id),
                x0: w.rect.x as f32,
                x1: w.rect.right() as f32,
                y: w.rect.y as f32,
                z: z as i32,
            });
        }
    }
    for out in &inputs.outputs {
        let bottom = out.rect.bottom();
        let mut covered: Vec<(i32, i32)> = inputs
            .obstacles
            .iter()
            .map(|o| o.rect)
            .filter(|r| r.y < bottom && r.bottom() >= bottom - 1)
            .map(|r| (r.x.max(out.rect.x), r.right().min(out.rect.right())))
            .filter(|(a, b)| a < b)
            .collect();
        covered.sort_unstable();
        let mut x = out.rect.x;
        let mut segment = 0;
        for (a, b) in covered
            .into_iter()
            .chain([(out.rect.right(), out.rect.right())])
        {
            if a - x >= 4 * COL_W as i32 {
                specs.push(Spec {
                    key: SurfaceKey::Ground {
                        output: out.name.clone(),
                        segment,
                    },
                    x0: x as f32,
                    x1: a as f32,
                    y: bottom as f32,
                    z: Z_GROUND,
                });
                segment += 1;
            }
            x = x.max(b);
        }
    }
    specs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{AppWindow, OutputInfo, WindowRect};

    fn layout(panel: Option<Rect>) -> Inputs {
        Inputs {
            outputs: vec![OutputInfo {
                name: "A".into(),
                rect: Rect::new(0, 0, 1920, 1080),
                scale: 1.0,
            }],
            obstacles: panel
                .map(|rect| WindowRect { id: 7, rect })
                .into_iter()
                .collect(),
            ..Inputs::default()
        }
    }

    const BOTTOM_PANEL: Rect = Rect::new(0, 1040, 1920, 40);

    fn sync(p: &mut Piles, inputs: &Inputs, now: f64, parts: &mut Particles) {
        p.sync(inputs, 40.0, 1.0, now, parts);
    }

    #[test]
    fn bottom_panel_replaces_the_ground() {
        let s = specs(&layout(Some(BOTTOM_PANEL)), 40.0);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].key, SurfaceKey::Obstacle(7));
        assert_eq!((s[0].x0, s[0].x1, s[0].y), (0.0, 1920.0, 1040.0));
    }

    #[test]
    fn short_panel_leaves_ground_on_both_sides() {
        let s = specs(&layout(Some(Rect::new(600, 1040, 700, 40))), 40.0);
        let ground: Vec<_> = s
            .iter()
            .filter(|s| matches!(s.key, SurfaceKey::Ground { .. }))
            .map(|s| (s.x0, s.x1, s.y))
            .collect();
        assert_eq!(ground, [(0.0, 600.0, 1080.0), (1300.0, 1920.0, 1080.0)]);
    }

    #[test]
    fn top_panel_collects_nothing() {
        let s = specs(&layout(Some(Rect::new(0, 0, 1920, 30))), 40.0);
        assert!(s.iter().all(|s| s.key != SurfaceKey::Obstacle(7)));
        assert_eq!(s.len(), 1, "only the ground");
    }

    fn piles_with_panel() -> Piles {
        let mut p = Piles::default();
        sync(
            &mut p,
            &layout(Some(BOTTOM_PANEL)),
            0.0,
            &mut Particles::new(0),
        );
        p
    }

    #[test]
    fn flakes_land_on_the_pile_top() {
        let mut p = piles_with_panel();
        assert!(!p.try_land(100.0, 1000.0, 1001.0, 2.0, 40.0), "still above");
        assert!(p.try_land(100.0, 1037.0, 1038.5, 2.0, 40.0));
        assert!(p.volume() > 0.0);
        // Below the surface already: no landing (it fell past behind something).
        assert!(!p.try_land(100.0, 1050.0, 1051.0, 2.0, 40.0));
    }

    #[test]
    fn slumping_conserves_snow_and_limits_slope() {
        let mut p = piles_with_panel();
        p.surfaces[0].heights[300] = 35.0;
        let before = p.volume();
        let mut parts = Particles::new(0);
        for _ in 0..5000 {
            p.step(1.0 / 60.0, 0.0, 40.0, &mut parts, &HashMap::new());
        }
        assert!(
            (p.volume() - before).abs() < 1e-2,
            "{} vs {before}",
            p.volume()
        );
        assert!(parts.is_empty(), "panels don't spill");
        let h = &p.surfaces[0].heights;
        let steepest = h
            .windows(2)
            .map(|w| (w[0] - w[1]).abs())
            .fold(0.0, f32::max);
        assert!(steepest <= MAX_STEP + 0.05, "steepest step {steepest}");
    }

    #[test]
    fn melting_shrinks_and_never_goes_negative() {
        let mut p = piles_with_panel();
        p.surfaces[0].heights.fill(10.0);
        let before = p.volume();
        let mut parts = Particles::new(0);
        for _ in 0..600 {
            p.step(1.0 / 60.0, 1.0, 40.0, &mut parts, &HashMap::new());
        }
        assert!(p.volume() < before);
        p.step(1e6, 1.0, 40.0, &mut parts, &HashMap::new());
        assert_eq!(p.volume(), 0.0);
    }

    #[test]
    fn height_never_exceeds_max() {
        let mut p = piles_with_panel();
        let mut parts = Particles::new(0);
        for i in 0..20_000 {
            let x = 900.0 + (i % 40) as f32;
            p.try_land(x, 990.0, 1045.0, 4.0, 25.0);
            p.step(1.0 / 60.0, 0.0, 25.0, &mut parts, &HashMap::new());
        }
        assert!(p.surfaces[0].max_height() <= 25.0);
        assert!(p.surfaces[0].max_height() > 20.0);
    }

    #[test]
    fn layout_changes_keep_snow() {
        let mut p = piles_with_panel();
        let mut parts = Particles::new(0);
        p.surfaces[0].heights[100] = 12.0;
        // Panel grows taller: same surface, moved up.
        sync(
            &mut p,
            &layout(Some(Rect::new(0, 1030, 1920, 50))),
            1.0,
            &mut parts,
        );
        assert_eq!(p.surfaces().len(), 1);
        assert_eq!(p.surfaces[0].y, 1030.0);
        assert_eq!(p.surfaces[0].heights[100], 12.0);
        // Panel removed: its snow is gone, the ground appears.
        sync(&mut p, &layout(None), 2.0, &mut parts);
        assert!(matches!(p.surfaces[0].key, SurfaceKey::Ground { .. }));
        assert_eq!(p.volume(), 0.0);
    }

    fn with_window(rect: Rect, collects: bool) -> Inputs {
        let mut i = layout(Some(BOTTOM_PANEL));
        i.windows.push(AppWindow {
            id: 42,
            rect,
            collects,
            pid: None,
        });
        i
    }

    fn window_pile(p: &Piles) -> Option<&Surface> {
        p.surfaces.iter().find(|s| s.key == SurfaceKey::Window(42))
    }

    fn snowy_window(p: &mut Piles, parts: &mut Particles) -> f32 {
        sync(
            p,
            &with_window(Rect::new(400, 300, 600, 400), true),
            0.0,
            parts,
        );
        let s = p
            .surfaces
            .iter_mut()
            .find(|s| s.key == SurfaceKey::Window(42))
            .unwrap();
        s.heights.fill(10.0);
        s.volume()
    }

    #[test]
    fn window_tops_collect_unless_maximized_or_too_high() {
        let s = specs(&with_window(Rect::new(400, 300, 600, 400), true), 40.0);
        assert!(s
            .iter()
            .any(|s| s.key == SurfaceKey::Window(42) && s.y == 300.0));
        let s = specs(&with_window(Rect::new(400, 300, 600, 400), false), 40.0);
        assert!(s.iter().all(|s| s.key != SurfaceKey::Window(42)));
        let s = specs(&with_window(Rect::new(400, 20, 600, 400), true), 40.0);
        assert!(
            s.iter().all(|s| s.key != SurfaceKey::Window(42)),
            "no room above"
        );
    }

    #[test]
    fn slow_moves_carry_snow_fast_moves_throw_it() {
        let (mut p, mut parts) = (Piles::default(), Particles::new(0));
        let vol = snowy_window(&mut p, &mut parts);
        // 3 px in 1/30 s = 90 px/s: rides along.
        sync(
            &mut p,
            &with_window(Rect::new(403, 300, 600, 400), true),
            1.0 / 30.0,
            &mut parts,
        );
        let s = window_pile(&p).unwrap();
        assert_eq!(s.x0, 403.0);
        assert!((s.volume() - vol).abs() < 1e-3);
        assert!(parts.is_empty());
        // 40 px in 1/30 s = 1200 px/s: thrown off to the right.
        sync(
            &mut p,
            &with_window(Rect::new(443, 300, 600, 400), true),
            2.0 / 30.0,
            &mut parts,
        );
        assert_eq!(window_pile(&p).unwrap().volume(), 0.0);
        assert!((parts.volume() - vol).abs() < PARTICLE_VOLUME + 1.0);
        let mean_vx = parts.iter().map(|q| q.vx).sum::<f32>() / parts.len() as f32;
        assert!(mean_vx > 500.0, "thrown with the window: {mean_vx}");
    }

    #[test]
    fn maximize_drops_snow_minimize_parks_it() {
        let (mut p, mut parts) = (Piles::default(), Particles::new(0));
        let vol = snowy_window(&mut p, &mut parts);
        // Minimized: parked, nothing falls.
        let mut hidden = layout(Some(BOTTOM_PANEL));
        hidden.hidden_windows.push(42);
        sync(&mut p, &hidden, 10.0, &mut parts);
        assert!(window_pile(&p).is_none());
        assert!(parts.is_empty());
        // Restored a minute later: back, minus a bit of melting.
        sync(
            &mut p,
            &with_window(Rect::new(400, 300, 600, 400), true),
            70.0,
            &mut parts,
        );
        let back = window_pile(&p).unwrap().volume();
        assert!(back < vol && back > vol * 0.3, "{back} of {vol}");
        // Maximized: it all falls as particles.
        sync(
            &mut p,
            &with_window(Rect::new(0, 0, 1920, 1040), false),
            71.0,
            &mut parts,
        );
        assert!(window_pile(&p).is_none());
        assert!((parts.volume() - back).abs() < PARTICLE_VOLUME + 1.0);
    }

    #[test]
    fn hot_windows_melt_faster_and_drip() {
        let melt_for = |load: f32| {
            let (mut p, mut parts) = (Piles::default(), Particles::new(0));
            let before = snowy_window(&mut p, &mut parts);
            let heat = HashMap::from([(42u64, load)]);
            for _ in 0..600 {
                p.step(1.0 / 60.0, 1.0, 40.0, &mut parts, &heat);
            }
            (before - window_pile(&p).unwrap().volume(), parts.len())
        };
        let (cold, cold_drips) = melt_for(0.0);
        let (hot, hot_drips) = melt_for(1.0);
        assert!(hot > cold * 5.0, "hot {hot} vs cold {cold}");
        // Both spill over the edges; the hot one also drips.
        assert!(
            hot_drips > cold_drips,
            "{hot_drips} vs {cold_drips} particles"
        );
    }

    #[test]
    fn snow_spills_over_window_edges() {
        let (mut p, mut parts) = (Piles::default(), Particles::new(0));
        sync(
            &mut p,
            &with_window(Rect::new(400, 300, 600, 400), true),
            0.0,
            &mut parts,
        );
        let s = p
            .surfaces
            .iter_mut()
            .find(|s| s.key == SurfaceKey::Window(42))
            .unwrap();
        s.heights[..10].fill(20.0);
        let before = p.volume();
        for _ in 0..600 {
            p.step(1.0 / 60.0, 0.0, 40.0, &mut parts, &HashMap::new());
        }
        let lost = before - p.volume();
        assert!(lost > PARTICLE_VOLUME, "only {lost} px² spilled");
        assert!(!parts.is_empty());
        assert!(
            parts.iter().all(|q| q.x < 400.0),
            "spilled off the left edge"
        );
    }
}
