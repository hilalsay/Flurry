//! Loose snow in flight: knocked off a dragged window, spilled over a
//! window's edge, or dropped when a window closes or maximizes. Particles
//! fall with gravity and air drag and settle on piles below.

use crate::geom::Rect;

/// Gravity, px/s².
const GRAVITY: f32 = 520.0;
/// Fraction of velocity lost per second to air drag.
const DRAG: f32 = 0.6;
/// Snow volume per particle when breaking up a pile (px²).
pub const PARTICLE_VOLUME: f32 = 36.0;
/// Upper bound on live particles; beyond it, new ones carry more volume.
const MAX_PARTICLES: usize = 3000;
/// A particle carrying at least this much snow (px²) is a lump: it is drawn
/// as a clump and bursts into powder where it lands.
pub const LUMP_VOLUME: f32 = 100.0;
/// Snow per lump when a closed window's pile drops (px²).
pub const DROP_LUMP_VOLUME: f32 = 170.0;
/// Share of a landing lump's snow that bursts up as powder; the rest settles.
pub const BURST_SHARE: f32 = 0.35;
/// Powder grains per burst.
const BURST_GRAINS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Particle {
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// Snow it carries (px²); also sets its drawn size.
    pub volume: f32,
}

impl Particle {
    /// Drawn radius (a lump's whole clump; see [`Particle::clump`]).
    pub fn radius(&self) -> f32 {
        let r = (self.volume / std::f32::consts::PI).sqrt();
        if self.is_lump() {
            r.clamp(4.0, 9.0)
        } else {
            r.clamp(1.2, 4.0)
        }
    }

    pub fn is_lump(&self) -> bool {
        self.volume >= LUMP_VOLUME
    }

    /// The discs (dx, dy, r) a lump is drawn as, so it looks clumpy rather
    /// than round; a plain particle is one disc.
    pub fn clump(&self) -> Vec<(f32, f32, f32)> {
        if !self.is_lump() {
            return vec![(0.0, 0.0, self.radius())];
        }
        let r = self.radius();
        // Tumbles a little as it falls.
        let turn = (self.x * 0.05 + self.y * 0.03).sin() * 0.6;
        let mut discs = vec![(0.0, 0.0, 4.0f32.min(r * 0.6))];
        for (k, s) in [(0.0, 0.95), (2.1, 0.8), (4.0, 0.9), (5.2, 0.7)] {
            let a: f32 = k + turn;
            let d = r - 3.0;
            discs.push((a.cos() * d, a.sin() * d * 0.8, 3.0 + s));
        }
        discs
    }
}

#[derive(Debug, Clone)]
pub struct Particles {
    list: Vec<Particle>,
    rng: fastrand::Rng,
}

impl Particles {
    pub fn new(seed: u64) -> Self {
        Self {
            list: Vec::new(),
            rng: fastrand::Rng::with_seed(seed),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Particle> {
        self.list.iter()
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn clear(&mut self) {
        self.list.clear();
    }

    /// Total snow in flight (px²).
    pub fn volume(&self) -> f32 {
        self.list.iter().map(|p| p.volume).sum()
    }

    /// A landed lump at (`x`, `y`) bursts: `volume` of powder flies up and
    /// out as small grains, which then settle again.
    pub fn burst(&mut self, x: f32, y: f32, volume: f32) {
        let grain = volume / BURST_GRAINS as f32;
        for _ in 0..BURST_GRAINS {
            let side = (self.rng.f32() - 0.5) * 2.0;
            self.list.push(Particle {
                x: x + side * 4.0,
                y: y - 2.0,
                vx: side * 170.0,
                vy: -(90.0 + self.rng.f32() * 150.0),
                volume: grain,
            });
        }
    }

    /// Add `p`, jittering its velocity by up to `spread` px/s.
    pub fn spawn(&mut self, mut p: Particle, spread: f32) {
        p.vx += (self.rng.f32() - 0.5) * 2.0 * spread;
        p.vy += (self.rng.f32() - 0.5) * 2.0 * spread;
        if self.list.len() >= MAX_PARTICLES {
            // Too many: fold the snow into a random live particle instead.
            let i = self.rng.usize(..self.list.len());
            self.list[i].volume += p.volume;
            return;
        }
        self.list.push(p);
    }

    /// Move everything; `land(particle, y_before)` may claim a particle (it
    /// settled). Particles leaving `bounds` (with some slack) vanish. With
    /// `confine` (the monitors), a particle can't leave the monitor it is
    /// on sideways: it stops at that monitor's edge.
    pub fn step(
        &mut self,
        dt: f32,
        bounds: &Rect,
        confine: Option<&[Rect]>,
        mut land: impl FnMut(&Particle, f32) -> bool,
    ) {
        let (left, right) = (bounds.x as f32 - 60.0, bounds.right() as f32 + 60.0);
        let bottom = bounds.bottom() as f32 + 20.0;
        let drag = (1.0 - DRAG * dt).max(0.0);
        self.list.retain_mut(|p| {
            let y_before = p.y;
            let home = confine.and_then(|outs| {
                outs.iter()
                    .find(|o| o.contains(p.x as i32, p.y.max(o.y as f32) as i32))
                    .copied()
            });
            p.vy += GRAVITY * dt;
            p.vx *= drag;
            p.vy *= drag;
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            if let Some(o) = home {
                let (l, r) = (o.x as f32, o.right() as f32 - 1.0);
                if p.x < l || p.x > r {
                    p.x = p.x.clamp(l, r);
                    p.vx = 0.0;
                }
            }
            let gone = p.y > bottom || p.x < left || p.x > right;
            !(gone || (p.vy > 0.0 && land(p, y_before)))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn particles_fall_and_leave_the_screen() {
        let bounds = Rect::new(0, 0, 800, 600);
        let mut ps = Particles::new(1);
        ps.spawn(
            Particle {
                x: 400.0,
                y: 100.0,
                vx: 300.0,
                vy: -100.0,
                volume: 30.0,
            },
            0.0,
        );
        let mut max_x = 0.0f32;
        for _ in 0..600 {
            ps.step(1.0 / 60.0, &bounds, None, |_, _| false);
            if let Some(p) = ps.iter().next() {
                max_x = max_x.max(p.x);
            }
        }
        assert!(ps.is_empty(), "fell off the bottom");
        assert!(max_x > 500.0, "carried sideways by its velocity ({max_x})");
    }

    #[test]
    fn landing_claims_particles_and_cap_keeps_volume() {
        let bounds = Rect::new(0, 0, 800, 600);
        let mut ps = Particles::new(2);
        for _ in 0..MAX_PARTICLES + 100 {
            ps.spawn(
                Particle {
                    x: 10.0,
                    y: 10.0,
                    vx: 0.0,
                    vy: 0.0,
                    volume: 1.0,
                },
                5.0,
            );
        }
        assert_eq!(ps.len(), MAX_PARTICLES);
        assert!((ps.volume() - (MAX_PARTICLES + 100) as f32).abs() < 1e-2);
        ps.step(1.0 / 60.0, &bounds, None, |_, _| true);
        assert!(ps.is_empty());
    }
}
