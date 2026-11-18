//! Snow globe (feature 6): shake the mouse quickly back and forth and the
//! settled snow near the pointer whirls up into a flurry, then falls and
//! settles again.

use std::collections::VecDeque;

use crate::geom::CursorState;
use crate::sim::particles::{Particle, Particles, PARTICLE_VOLUME};
use crate::sim::piles::{Piles, COL_W};

/// A direction change counts if the pointer moved at least this fast (px/s).
const SHAKE_SPEED: f32 = 900.0;
/// This many direction changes within `WINDOW` seconds is a shake.
const REVERSALS: usize = 4;
const WINDOW: f64 = 0.8;
/// Snow within this distance of the pointer is lifted (px).
const RADIUS: f32 = 260.0;
/// Share of the snow lifted.
const LIFT: f32 = 0.7;
const COOLDOWN: f64 = 1.5;

#[derive(Debug, Clone, Default)]
pub struct Globe {
    last: Option<(f32, f64)>,
    last_dir: f32,
    reversals: VecDeque<f64>,
    ready_at: f64,
}

impl Globe {
    /// Returns true when a shake was detected (and the snow lifted).
    pub fn step(
        &mut self,
        now: f64,
        cursor: Option<CursorState>,
        piles: &mut Piles,
        particles: &mut Particles,
    ) -> bool {
        let Some(c) = cursor else {
            self.last = None;
            return false;
        };
        let Some((px, pt)) = self.last else {
            self.last = Some((c.x, now));
            return false;
        };
        if c.x == px {
            return false; // no new sample
        }
        let vx = (c.x - px) / ((now - pt) as f32).max(1.0 / 240.0);
        self.last = Some((c.x, now));
        if vx.abs() >= SHAKE_SPEED {
            let dir = vx.signum();
            if self.last_dir != 0.0 && dir != self.last_dir {
                self.reversals.push_back(now);
            }
            self.last_dir = dir;
        }
        while self.reversals.front().is_some_and(|&t| now - t > WINDOW) {
            self.reversals.pop_front();
        }
        if self.reversals.len() < REVERSALS || now < self.ready_at {
            return false;
        }
        self.reversals.clear();
        self.ready_at = now + COOLDOWN;
        lift(c.x, c.y, piles, particles);
        true
    }
}

/// Whirl the snow near (`x`, `y`) up into particles circling the pointer.
fn lift(x: f32, y: f32, piles: &mut Piles, particles: &mut Particles) {
    let keys: Vec<_> = piles
        .surfaces()
        .iter()
        .filter(|s| (s.y - y).abs() < RADIUS && s.x0 < x + RADIUS && s.x1() > x - RADIUS)
        .map(|s| (s.key.clone(), s.y))
        .collect();
    for (key, sy) in keys {
        let mut cx = x - RADIUS;
        while cx < x + RADIUS {
            let d = (cx - x).hypot(sy - y);
            if d < RADIUS {
                let taken = piles.take(&key, cx, COL_W * 2.0, f32::MAX) * LIFT;
                // Put back what we don't lift.
                if taken > 0.0 {
                    piles.land(
                        cx,
                        sy - 50.0,
                        sy + 1.0,
                        1.0,
                        taken / LIFT * (1.0 - LIFT),
                        f32::MAX,
                    );
                }
                let mut left = taken;
                while left > 1.0 {
                    let v = left.min(PARTICLE_VOLUME);
                    left -= v;
                    // Up, and around the pointer (counter-clockwise).
                    let (dx, dy) = (cx - x, sy - y);
                    let n = dx.hypot(dy).max(1.0);
                    let swirl = 320.0 * (1.0 - d / RADIUS).max(0.3);
                    particles.spawn(
                        Particle {
                            x: cx,
                            y: sy - 3.0,
                            vx: -dy / n * swirl,
                            vy: dx / n * swirl - 380.0,
                            volume: v,
                        },
                        120.0,
                    );
                }
            }
            cx += COL_W * 4.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{Inputs, OutputInfo, Rect, WindowRect};

    fn snowy_panel() -> Piles {
        let inputs = Inputs {
            outputs: vec![OutputInfo {
                name: "A".into(),
                rect: Rect::new(0, 0, 1920, 1080),
                scale: 1.0,
            }],
            obstacles: vec![WindowRect {
                id: 1,
                rect: Rect::new(0, 1040, 1920, 40),
            }],
            ..Inputs::default()
        };
        let mut p = Piles::default();
        p.sync(&inputs, 40.0, 1.0, 0.0, &mut Particles::new(0));
        for x in (0..1920).step_by(4) {
            p.land(x as f32, 1000.0, 1050.0, 1.0, 40.0, 40.0);
        }
        p
    }

    /// Wiggle the pointer around x = 900 for a second (3 Hz sine, peak
    /// speed `speed` px/s), sampled at 60 Hz.
    fn sweep(g: &mut Globe, p: &mut Piles, parts: &mut Particles, speed: f32) -> bool {
        let f = 3.0;
        let amp = speed / (std::f32::consts::TAU * f);
        let mut shook = false;
        for i in 1..=60 {
            let t = i as f64 / 60.0;
            let c = CursorState {
                x: 900.0 + amp * (std::f32::consts::TAU * f * t as f32).sin(),
                y: 1000.0,
                ..CursorState::default()
            };
            shook |= g.step(t, Some(c), p, parts);
        }
        shook
    }

    #[test]
    fn a_fast_shake_lifts_the_snow_near_the_pointer() {
        let (mut g, mut p, mut parts) = (Globe::default(), snowy_panel(), Particles::new(0));
        let before = p.volume();
        assert!(sweep(&mut g, &mut p, &mut parts, 1500.0), "shake detected");
        assert!(parts.len() > 50, "{} particles", parts.len());
        let lifted = before - p.volume();
        assert!((lifted - parts.volume()).abs() < 5.0, "snow conserved");
        let s = &p.surfaces()[0];
        let near = s.top_at(900.0).unwrap();
        let far = s.top_at(100.0).unwrap();
        assert!(near > far, "snow near the pointer is lower than far away");
    }

    #[test]
    fn slow_wiggles_do_nothing() {
        let (mut g, mut p, mut parts) = (Globe::default(), snowy_panel(), Particles::new(0));
        assert!(!sweep(&mut g, &mut p, &mut parts, 300.0));
        assert!(parts.is_empty());
    }
}
