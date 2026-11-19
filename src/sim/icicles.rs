//! Icicles (feature 2): they grow slowly from the bottom edge of a window
//! whose top has had snow for a while (not from the panel: the user found
//! them distracting there). They drip now and then. Flinging the window
//! snaps them off, and so does a fast mouse swipe through them (they regrow
//! while the snow stays): they fall and shatter into loose snow. Without
//! snow they slowly melt away.

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
/// A cursor moving at least this fast (px/s) knocks icicles off.
const SWIPE_SPEED: f32 = 500.0;
/// Extra px around an icicle that the swipe still hits.
const SWIPE_MARGIN: f32 = 3.0;

/// What an edge belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKey {
    Window(u64),
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
    /// Stacking depth of its window.
    z: i32,
}

/// An edge that can grow icicles now: its key, the pile that feeds it, its
/// stacking depth, and where it hangs (left x, y, width).
type EdgeNow = (EdgeKey, SurfaceKey, i32, (f32, f32, f32));

/// A cursor path: from, to, velocity.
type SwipePath = ((f32, f32), (f32, f32), (f32, f32));

/// Does any point along the segment `from`–`to` (every 2 px) satisfy `hit`?
fn swipe_point(from: (f32, f32), to: (f32, f32), hit: impl Fn(f32, f32) -> bool) -> bool {
    let len = (to.0 - from.0).hypot(to.1 - from.1);
    let n = (len / 2.0).ceil().max(1.0) as usize;
    (0..=n).any(|i| {
        let t = i as f32 / n as f32;
        hit(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t)
    })
}

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
    /// Simulated time (s), and the last cursor position that differed
    /// from the one before, with when it was seen.
    clock: f32,
    cursor: Option<(f32, f32, f32)>,
}

impl Default for Icicles {
    fn default() -> Self {
        Self {
            edges: HashMap::new(),
            falling: Vec::new(),
            next_falling: 1 << 48,
            rng: fastrand::Rng::with_seed(0x01C1_C1E5),
            clock: 0.0,
            cursor: None,
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
                EdgeKey::Window(id) => *id,
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
        self.swipe(dt, inputs);
        self.fall(dt, piles, particles, bounds);
    }

    /// The cursor's path since its last move, if it moved fast enough to
    /// knock icicles off.
    fn track_swipe(&mut self, dt: f32, inputs: &Inputs) -> Option<SwipePath> {
        self.clock += dt;
        let c = inputs.cursor.as_ref()?;
        let (x, y) = (c.x, c.y);
        let Some((px, py, t)) = self.cursor else {
            self.cursor = Some((x, y, self.clock));
            return None;
        };
        if (px, py) == (x, y) {
            return None;
        }
        self.cursor = Some((x, y, self.clock));
        let elapsed = (self.clock - t).max(1.0 / 120.0);
        let (vx, vy) = ((x - px) / elapsed, (y - py) / elapsed);
        (vx.hypot(vy) >= SWIPE_SPEED).then_some(((px, py), (x, y), (vx, vy)))
    }

    /// Knock off every hanging icicle a fast cursor swipe went through,
    /// unless a window above it hides it there.
    fn swipe(&mut self, dt: f32, inputs: &Inputs) {
        let Some((from, to, v)) = self.track_swipe(dt, inputs) else {
            return;
        };
        let hidden = |x: f32, y: f32, z: i32| {
            inputs
                .windows
                .iter()
                .enumerate()
                .any(|(wz, w)| wz as i32 > z && w.rect.contains(x as i32, y as i32))
        };
        let mut knocked = Vec::new();
        for e in self.edges.values_mut() {
            let (x0, y, _) = e.at;
            for s in e.seeds.iter_mut().filter(|s| s.length >= 1.0) {
                let (ix, half) = (x0 + s.offset, s.width / 2.0 + SWIPE_MARGIN);
                let hit = swipe_point(from, to, |px, py| {
                    (px - ix).abs() <= half
                        && py >= y - SWIPE_MARGIN
                        && py <= y + s.length + SWIPE_MARGIN
                        && !hidden(px, py, e.z)
                });
                if hit {
                    knocked.push((ix, y, s.length, s.width));
                    s.length = 0.0;
                }
            }
        }
        for (x, y, length, width) in knocked {
            self.next_falling += 1;
            self.falling.push(Falling {
                icicle: Icicle {
                    id: self.next_falling,
                    x,
                    y,
                    length,
                    width,
                    z: i32::MAX,
                },
                vx: (v.0 * 0.15).clamp(-250.0, 250.0),
                vy: (v.1 * 0.15).clamp(-100.0, 150.0),
            });
        }
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

    #[test]
    fn no_icicles_on_the_panel() {
        let panel = Rect::new(0, 1040, 1920, 40);
        let inp = Inputs {
            obstacles: vec![crate::geom::WindowRect { id: 1, rect: panel }],
            windows: Vec::new(),
            ..inputs(Rect::new(0, 0, 1, 1))
        };
        let (mut ice, mut piles, mut parts) =
            (Icicles::default(), Piles::default(), Particles::new(0));
        piles.sync(&inp, 40.0, 1.0, 0.0, &mut parts);
        for x in (0..1920).step_by(3) {
            piles.land(x as f32, 990.0, 1045.0, 1.0, 40.0, 40.0);
        }
        for _ in 0..60 * 120 {
            ice.step(
                1.0 / 60.0,
                &inp,
                &mut piles,
                &mut parts,
                Rect::new(0, 0, 1920, 1080),
            );
        }
        assert!(ice.all().is_empty());
    }

    #[test]
    fn a_fast_swipe_knocks_icicles_off_a_slow_one_does_not() {
        let win = Rect::new(400, 300, 600, 400);
        let bounds = Rect::new(0, 0, 1920, 1080);
        let (mut ice, mut piles, mut parts) =
            (Icicles::default(), Piles::default(), Particles::new(0));
        let mut inp = inputs(win);
        snowy(&mut piles, &inp, &mut parts);
        for _ in 0..60 * 90 {
            ice.step(1.0 / 60.0, &inp, &mut piles, &mut parts, bounds);
        }
        let hanging = ice.all().len();
        assert!(hanging >= 5);
        let at = |x: f32| {
            Some(crate::geom::CursorState {
                x,
                y: 702.0,
                ..Default::default()
            })
        };
        let mut run = |ice: &mut Icicles, inp: &mut Inputs, from: f32, to: f32, speed: f32| {
            let mut x = from;
            while x < to {
                x += speed / 60.0;
                inp.cursor = at(x);
                ice.step(1.0 / 60.0, inp, &mut piles, &mut parts, bounds);
            }
        };
        // Slowly along the bottom edge: nothing falls.
        run(&mut ice, &mut inp, 380.0, 1020.0, 200.0);
        assert!(ice.all().iter().all(|i| i.z != i32::MAX), "slow: none fell");
        // A fast swipe along the edge: they all snap off and fall.
        run(&mut ice, &mut inp, 380.0, 1020.0, 1500.0);
        let all = ice.all();
        let falling = all.iter().filter(|i| i.z == i32::MAX).count();
        assert_eq!(falling, hanging, "fast: all knocked off");
    }
}
