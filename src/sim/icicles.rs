//! Icicles (feature 2): they grow slowly from the bottom edge of a window
//! whose top has had snow for a while, and from the top edge of a snowy
//! panel. They drip now and then. Flinging the window snaps them off: they
//! fall and shatter into loose snow. Without snow they slowly melt away.

use std::collections::HashMap;

use crate::geom::{Inputs, Rect};
use crate::sim::particles::{Particle, Particles};
use crate::sim::piles::{Piles, SurfaceKey};

/// Seconds of snow on top before icicles start.
const DELAY: f32 = 40.0;
/// Snow on top needed to count as snowy (px²).
const SNOWY_VOLUME: f32 = 300.0;
/// Growth while snowy and melting otherwise (px/s).
const GROW: f32 = 0.25;
const MELT: f32 = 0.15;
/// Roughly one icicle per this many px of edge.
const SPACING: f32 = 55.0;
const DRIPS_PER_SEC: f32 = 0.03;
const GRAVITY: f32 = 700.0;

/// What an edge belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKey {
    Window(u64),
    Panel(u64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Seed {
    /// Position along the edge, from its left end (px).
    offset: f32,
    length: f32,
    max_length: f32,
    width: f32,
}

#[derive(Debug, Clone)]
struct Edge {
    seeds: Vec<Seed>,
    snowy_for: f32,
    /// Where the edge hangs (global): left x, y, width.
    at: (f32, f32, f32),
    /// Stacking depth of its window/panel.
    z: i32,
}

/// An edge that can grow icicles now: its key, the pile that feeds it, its
/// stacking depth, and where it hangs (left x, y, width).
type EdgeNow = (EdgeKey, SurfaceKey, i32, (f32, f32, f32));

/// One icicle to draw, in global coordinates: it hangs from (`x`, `y`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Icicle {
    /// Stable id while it hangs (edge + index); falling ones count up from 1<<48.
    pub id: u64,
    pub x: f32,
    pub y: f32,
    pub length: f32,
    pub width: f32,
    /// Stacking depth of what it hangs from (for occlusion), like `Surface::z`.
    pub z: i32,
}

#[derive(Debug, Clone, Copy)]
struct Falling {
    icicle: Icicle,
    vx: f32,
    vy: f32,
}

#[derive(Debug, Clone)]
pub struct Icicles {
    edges: HashMap<EdgeKey, Edge>,
    falling: Vec<Falling>,
    next_falling: u64,
    rng: fastrand::Rng,
}

impl Default for Icicles {
    fn default() -> Self {
        Self {
            edges: HashMap::new(),
            falling: Vec::new(),
            next_falling: 1 << 48,
            rng: fastrand::Rng::with_seed(0x01C1_C1E5),
        }
    }
}

impl Icicles {
    pub fn clear(&mut self) {
        self.edges.clear();
        self.falling.clear();
    }

    /// Everything to draw: hanging and falling icicles.
    pub fn all(&self) -> Vec<Icicle> {
        let mut out = Vec::new();
        for (key, e) in &self.edges {
            let (x0, y, _) = e.at;
            let tag = match key {
                EdgeKey::Window(id) | EdgeKey::Panel(id) => *id,
            };
            for (i, s) in e.seeds.iter().enumerate() {
                if s.length >= 1.0 {
                    out.push(Icicle {
                        id: (tag << 8) | i as u64,
                        x: x0 + s.offset,
                        y,
                        length: s.length,
                        width: s.width,
                        z: e.z,
                    });
                }
            }
        }
        out.extend(self.falling.iter().map(|f| f.icicle));
        out
    }

    /// Edges that grow icicles now, with the pile that feeds them, their
    /// stacking depth, and where they hang.
    fn edges_now(inputs: &Inputs) -> Vec<EdgeNow> {
        let mut v = Vec::new();
        for (z, w) in inputs.windows.iter().enumerate() {
            if w.collects {
                let r = w.rect;
                v.push((
                    EdgeKey::Window(w.id),
                    SurfaceKey::Window(w.id),
                    z as i32,
                    (r.x as f32, r.bottom() as f32, r.w as f32),
                ));
            }
        }
        for o in &inputs.obstacles {
            let r = o.rect;
            v.push((
                EdgeKey::Panel(o.id),
                SurfaceKey::Obstacle(o.id),
                crate::sim::piles::Z_PANEL,
                (r.x as f32, r.y as f32, r.w as f32),
            ));
        }
        v
    }

    pub fn step(
        &mut self,
        dt: f32,
        inputs: &Inputs,
        piles: &mut Piles,
        particles: &mut Particles,
        bounds: Rect,
    ) {
        let knocked: Vec<(SurfaceKey, (f32, f32))> = piles.knocked().to_vec();
        let now = Self::edges_now(inputs);
        // Edges that went away (closed, maximized): their icicles fall.
        let alive: Vec<EdgeKey> = now.iter().map(|e| e.0).collect();
        let gone: Vec<EdgeKey> = self
            .edges
            .keys()
            .filter(|k| !alive.contains(k))
            .copied()
            .collect();
        for k in gone {
            if let Some(e) = self.edges.remove(&k) {
                self.drop_all(&e, (0.0, 0.0));
            }
        }
        for (key, pile, z, at) in now {
            let snowy = piles
                .surface(&pile)
                .is_some_and(|s| s.volume() > SNOWY_VOLUME);
            let flung = knocked.iter().find(|(k, _)| *k == pile).map(|(_, v)| *v);
            let edge = self.edges.entry(key).or_insert(Edge {
                seeds: Vec::new(),
                snowy_for: 0.0,
                at,
                z,
            });
            (edge.at, edge.z) = (at, z);
            if let Some(v) = flung {
                let e = std::mem::replace(
                    edge,
                    Edge {
                        seeds: Vec::new(),
                        snowy_for: 0.0,
                        at,
                        z,
                    },
                );
                self.drop_all(&e, (v.0 * 0.5, v.1 * 0.5));
                continue;
            }
            edge.snowy_for = if snowy {
                edge.snowy_for + dt
            } else {
                (edge.snowy_for - dt).max(0.0)
            };
            if edge.seeds.is_empty() && edge.snowy_for > DELAY {
                let n = ((at.2 / SPACING) as usize).max(1);
                edge.seeds = (0..n)
                    .map(|i| Seed {
                        offset: (i as f32 + 0.2 + self.rng.f32() * 0.6) * at.2 / n as f32,
                        length: 0.0,
                        max_length: 6.0 + self.rng.f32() * 24.0,
                        width: 2.5 + self.rng.f32() * 3.5,
                    })
                    .collect();
            }
            for s in &mut edge.seeds {
                if snowy {
                    s.length = (s.length + GROW * dt).min(s.max_length);
                } else {
                    s.length = (s.length - MELT * dt).max(0.0);
                }
                if s.length > 5.0 && self.rng.f32() < DRIPS_PER_SEC * dt {
                    particles.spawn(
                        Particle {
                            x: at.0 + s.offset,
                            y: at.1 + s.length + 1.0,
                            vx: 0.0,
                            vy: 20.0,
                            volume: 1.5,
                        },
                        1.0,
                    );
                }
            }
            if !snowy && edge.snowy_for == 0.0 && edge.seeds.iter().all(|s| s.length == 0.0) {
                edge.seeds.clear();
            }
        }
        self.fall(dt, piles, particles, bounds);
    }

    fn drop_all(&mut self, e: &Edge, v: (f32, f32)) {
        let (x0, y, _) = e.at;
        for s in e.seeds.iter().filter(|s| s.length >= 1.0) {
            self.next_falling += 1;
            self.falling.push(Falling {
                icicle: Icicle {
                    id: self.next_falling,
                    x: x0 + s.offset,
                    y,
                    length: s.length,
                    width: s.width,
                    z: i32::MAX,
                },
                vx: v.0,
                vy: v.1,
            });
        }
    }

    /// Falling icicles shatter into loose snow when they hit a pile or the
    /// bottom of the screen.
    fn fall(&mut self, dt: f32, piles: &Piles, particles: &mut Particles, bounds: Rect) {
        let bottom = bounds.bottom() as f32;
        let mut shattered = Vec::new();
        self.falling.retain_mut(|f| {
            let i = &mut f.icicle;
            let tip_before = i.y + i.length;
            f.vy += GRAVITY * dt;
            i.x += f.vx * dt;
            i.y += f.vy * dt;
            let tip = i.y + i.length;
            let hit = piles.landing(i.x, tip_before, tip).is_some() || tip >= bottom;
            if hit {
                shattered.push(*i);
            }
            !hit
        });
        for i in shattered {
            let pieces = (i.length / 4.0).ceil().max(1.0) as usize;
            for k in 0..pieces {
                particles.spawn(
                    Particle {
                        x: i.x,
                        y: i.y + i.length - 2.0,
                        vx: (k as f32 - pieces as f32 / 2.0) * 25.0,
                        vy: -90.0,
                        volume: i.width * 1.5,
                    },
                    30.0,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{AppWindow, OutputInfo};

    fn inputs(rect: Rect) -> Inputs {
        Inputs {
            outputs: vec![OutputInfo {
                name: "A".into(),
                rect: Rect::new(0, 0, 1920, 1080),
                scale: 1.0,
            }],
            windows: vec![AppWindow {
                id: 9,
                rect,
                collects: true,
                pid: None,
            }],
            ..Inputs::default()
        }
    }

    fn snowy(piles: &mut Piles, inputs: &Inputs, parts: &mut Particles) {
        piles.sync(inputs, 40.0, 1.0, 0.0, parts);
        for x in (400..1000).step_by(3) {
            piles.land(x as f32, 250.0, 305.0, 1.0, 40.0, 40.0);
        }
    }

    #[test]
    fn icicles_grow_on_snowy_windows_and_fall_when_flung() {
        let win = Rect::new(400, 300, 600, 400);
        let (mut ice, mut piles, mut parts) =
            (Icicles::default(), Piles::default(), Particles::new(0));
        let inp = inputs(win);
        snowy(&mut piles, &inp, &mut parts);
        for _ in 0..60 * 30 {
            ice.step(
                1.0 / 60.0,
                &inp,
                &mut piles,
                &mut parts,
                Rect::new(0, 0, 1920, 1080),
            );
        }
        assert!(ice.all().is_empty(), "not yet: icicles need a while");
        for _ in 0..60 * 60 {
            ice.step(
                1.0 / 60.0,
                &inp,
                &mut piles,
                &mut parts,
                Rect::new(0, 0, 1920, 1080),
            );
        }
        let hanging = ice.all();
        assert!(hanging.len() >= 5, "{} icicles", hanging.len());
        assert!(
            hanging.iter().all(|i| i.y == 700.0),
            "they hang from the bottom edge"
        );
        assert!(hanging.iter().any(|i| i.length > 4.0));
        // Fling the window (80 px in one frame): they snap off, fall, shatter.
        piles.sync(
            &inputs(Rect::new(401, 300, 600, 400)),
            40.0,
            1.0,
            90.0,
            &mut parts,
        );
        let moved = inputs(Rect::new(481, 300, 600, 400));
        piles.sync(&moved, 40.0, 1.0, 90.0 + 1.0 / 60.0, &mut parts);
        ice.step(
            1.0 / 60.0,
            &moved,
            &mut piles,
            &mut parts,
            Rect::new(0, 0, 1920, 1080),
        );
        assert!(ice.all().iter().all(|i| i.z == i32::MAX), "all falling");
        let before = parts.len();
        for _ in 0..120 {
            ice.step(
                1.0 / 60.0,
                &moved,
                &mut piles,
                &mut parts,
                Rect::new(0, 0, 1920, 1080),
            );
        }
        assert!(ice.all().is_empty(), "shattered");
        assert!(parts.len() > before, "into loose snow");
    }
}
