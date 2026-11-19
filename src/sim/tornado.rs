//! Tornado / snow storm (on demand: `flurry tornado` or the tray menu).
//!
//! For [`DURATION`] seconds the wind rises, snow falls much harder, and a
//! funnel of swirling snow sweeps across the screen. Falling flakes near it
//! are pulled in and carried up. Piles under it are sucked up and flung out
//! as loose snow, and snowballs and snowmen in its path are thrown.
//! Everything eases in and out, so nothing pops.

use std::f32::consts::TAU;

use crate::geom::Rect;
use crate::sim::flakes::Layer;

/// How long a storm lasts (s).
pub const DURATION: f32 = 30.0;
/// Swirling bits drawn along the funnel at full strength.
const BITS: usize = 520;
/// Extra flakes during the storm, at full strength (× the normal count).
pub const EXTRA_FLAKES: f32 = 0.8;
/// Storm wind at full strength (px/s, before each layer's wind factor).
const WIND: f32 = 150.0;
/// Pile snow sucked up per second at full strength (px²).
const SUCK_PER_SEC: f32 = 900.0;

/// The funnel at one moment: where it is and how big.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Funnel {
    /// x of its foot (global).
    pub x: f32,
    /// y of the ground it stands on, and its height (px).
    pub ground: f32,
    pub height: f32,
    /// 0..=1: how formed it is (it grows in and fades out).
    pub strength: f32,
    /// Time (s) since the storm began, for the sway.
    age: f32,
}

impl Funnel {
    /// Radius at relative height `h` (0 = ground, 1 = top): narrow at the
    /// foot, wide at the top.
    pub fn radius(&self, h: f32) -> f32 {
        (14.0 + 150.0 * h.clamp(0.0, 1.0).powf(1.5)) * (0.4 + 0.6 * self.strength)
    }

    /// Centre x at relative height `h`; the funnel bends and sways.
    pub fn centre(&self, h: f32) -> f32 {
        self.x + (self.age * 1.3 + h * 3.0).sin() * 35.0 * h
    }

    /// Relative height of global `y` (may be outside 0..=1).
    pub fn rel_height(&self, y: f32) -> f32 {
        (self.ground - y) / self.height
    }
}

#[derive(Debug, Clone, Copy)]
struct Bit {
    /// Relative height (0 = ground, 1 = top), angle around the axis.
    h: f32,
    theta: f32,
    omega: f32,
    rise: f32,
    r: f32,
}

/// One bit of the funnel to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwirlBit {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub layer: Layer,
}

#[derive(Debug, Clone)]
pub struct Storm {
    age: f32,
    /// +1: travels to the right, -1: to the left.
    dir: f32,
    bits: Vec<Bit>,
    rng: fastrand::Rng,
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Storm {
    pub fn new(seed: u64) -> Storm {
        let mut rng = fastrand::Rng::with_seed(seed);
        let dir = if rng.bool() { 1.0 } else { -1.0 };
        let bits = (0..BITS)
            .map(|_| Bit {
                h: rng.f32(),
                theta: rng.f32() * TAU,
                omega: 3.0 + rng.f32() * 3.0,
                rise: 0.06 + rng.f32() * 0.12,
                r: 1.6 + rng.f32() * 2.2,
            })
            .collect();
        Storm {
            age: 0.0,
            dir,
            bits,
            rng,
        }
    }

    pub fn is_over(&self) -> bool {
        self.age >= DURATION
    }

    /// Seconds left.
    pub fn remaining(&self) -> f32 {
        (DURATION - self.age).max(0.0)
    }

    /// 0..=1: how strong the storm (wind, snowfall) is now.
    pub fn strength(&self) -> f32 {
        smoothstep(0.0, 4.0, self.age) * (1.0 - smoothstep(DURATION - 5.0, DURATION, self.age))
    }

    /// Extra wind now (px/s), gusty.
    pub fn wind(&self) -> f32 {
        self.dir * WIND * self.strength() * (0.75 + 0.25 * (self.age * 0.9).sin())
    }

    /// The funnel, while it is there.
    pub fn funnel(&self, bounds: &Rect) -> Option<Funnel> {
        let strength = smoothstep(3.0, 7.0, self.age)
            * (1.0 - smoothstep(DURATION - 7.0, DURATION - 2.0, self.age));
        if strength <= 0.0 {
            return None;
        }
        // From 10% to 90% of the width over the storm, wandering a bit.
        let w = bounds.w as f32;
        let p = (self.age / DURATION).clamp(0.0, 1.0);
        let p = if self.dir > 0.0 { p } else { 1.0 - p };
        let x = bounds.x as f32 + w * (0.1 + 0.8 * p) + (self.age * 0.7).sin() * 60.0;
        Some(Funnel {
            x,
            ground: bounds.bottom() as f32,
            height: bounds.h as f32 * 0.75,
            strength,
            age: self.age,
        })
    }

    pub fn step(&mut self, dt: f32) {
        self.age += dt;
        let rng = &mut self.rng;
        for b in &mut self.bits {
            // Faster spin near the foot.
            b.theta = (b.theta + b.omega * (1.5 - 0.7 * b.h) * dt) % TAU;
            b.h += b.rise * dt;
            if b.h > 1.0 {
                b.h -= 1.0;
                b.theta = rng.f32() * TAU;
            }
        }
    }

    /// The funnel's swirling snow, to draw like flakes.
    pub fn bits(&self, bounds: &Rect) -> Vec<SwirlBit> {
        let Some(f) = self.funnel(bounds) else {
            return Vec::new();
        };
        let n = (self.bits.len() as f32 * f.strength) as usize;
        self.bits[..n]
            .iter()
            .map(|b| {
                let (s, c) = b.theta.sin_cos();
                // Front of the funnel is nearer (brighter, bigger).
                let layer = if c > 0.35 {
                    Layer::Near
                } else if c < -0.35 {
                    Layer::Far
                } else {
                    Layer::Mid
                };
                SwirlBit {
                    x: f.centre(b.h) + f.radius(b.h) * s,
                    y: f.ground - b.h * f.height,
                    r: b.r * (0.8 + 0.4 * c.max(0.0)),
                    layer,
                }
            })
            .collect()
    }

    /// Pile snow to suck up this step at the funnel's foot: where, how wide,
    /// and how much (px²).
    pub fn suction(&self, dt: f32, bounds: &Rect) -> Option<(Funnel, f32)> {
        let f = self.funnel(bounds)?;
        Some((f, SUCK_PER_SEC * f.strength * dt))
    }

    /// A random fling velocity for snow thrown out of the funnel.
    pub fn fling(&mut self) -> (f32, f32) {
        let side = if self.rng.bool() { 1.0 } else { -1.0 };
        (
            side * (120.0 + self.rng.f32() * 260.0) + self.dir * 80.0,
            -(260.0 + self.rng.f32() * 320.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Rect = Rect::new(0, 0, 1920, 1080);

    #[test]
    fn storm_eases_in_and_out_and_ends() {
        let mut s = Storm::new(1);
        assert_eq!(s.strength(), 0.0);
        assert!(s.funnel(&B).is_none(), "the funnel forms later");
        for _ in 0..60 * 15 {
            s.step(1.0 / 60.0);
        }
        assert!(s.strength() > 0.99, "full storm mid-way");
        let f = s.funnel(&B).expect("funnel mid-way");
        assert!(f.strength > 0.99);
        assert!(f.x > 0.0 && f.x < 1920.0);
        assert!(f.radius(1.0) > f.radius(0.0) * 4.0, "wide top, narrow foot");
        assert_eq!(s.bits(&B).len(), BITS);
        assert!(s.wind().abs() > 50.0);
        for _ in 0..60 * 16 {
            s.step(1.0 / 60.0);
        }
        assert!(s.is_over());
        assert_eq!(s.strength(), 0.0);
        assert!(s.bits(&B).is_empty());
    }

    #[test]
    fn the_funnel_crosses_the_screen() {
        let mut s = Storm::new(7);
        let mut xs = Vec::new();
        while !s.is_over() {
            s.step(1.0 / 60.0);
            if let Some(f) = s.funnel(&B) {
                xs.push(f.x);
            }
        }
        let (first, last) = (xs[0], *xs.last().unwrap());
        assert!((last - first).abs() > 900.0, "travels: {first} -> {last}");
        // Its bits stay on its funnel, inside the screen height.
        let mut s = Storm::new(7);
        for _ in 0..60 * 12 {
            s.step(1.0 / 60.0);
        }
        let f = s.funnel(&B).unwrap();
        for b in s.bits(&B) {
            let h = f.rel_height(b.y);
            assert!((-0.01..=1.01).contains(&h));
            assert!((b.x - f.centre(h)).abs() <= f.radius(h) + 0.5);
        }
    }
}
