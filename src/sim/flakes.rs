//! Falling flakes, depth layers and the wind field.

use std::f64::consts::{PI, TAU};

use crate::geom::Rect;

/// Flakes wrap horizontally this far beyond the virtual desktop's edges,
/// so they slide out of view instead of popping at the border.
pub const WRAP_MARGIN: f32 = 8.0;
/// Recycled flakes respawn up to this far above the top edge, so they
/// don't arrive in a visible row.
pub const SPAWN_HEIGHT: f32 = 40.0;

/// Depth layer. Nearer flakes are bigger, brighter, faster and react more to wind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Far,
    Mid,
    Near,
}

struct LayerSpec {
    radius: (f32, f32),
    alpha: f32,
    /// Fall speed range in px/s at `fall_speed = 1.0`.
    fall: (f32, f32),
    /// How strongly this layer follows the wind.
    wind: f32,
    /// Sway amplitude range in px/s.
    sway: (f32, f32),
    /// Share of all flakes in this layer.
    share: f32,
}

const FAR: LayerSpec = LayerSpec {
    radius: (0.8, 1.5),
    alpha: 0.45,
    fall: (32.0, 48.0),
    wind: 0.45,
    sway: (3.0, 8.0),
    share: 0.45,
};
const MID: LayerSpec = LayerSpec {
    radius: (1.5, 2.5),
    alpha: 0.7,
    fall: (55.0, 75.0),
    wind: 0.75,
    sway: (6.0, 14.0),
    share: 0.35,
};
const NEAR: LayerSpec = LayerSpec {
    radius: (2.5, 4.0),
    alpha: 0.9,
    fall: (85.0, 120.0),
    wind: 1.0,
    sway: (10.0, 22.0),
    share: 0.20,
};

impl Layer {
    /// Back to front, the order they are drawn in.
    pub const ALL: [Layer; 3] = [Layer::Far, Layer::Mid, Layer::Near];

    fn spec(self) -> &'static LayerSpec {
        match self {
            Layer::Far => &FAR,
            Layer::Mid => &MID,
            Layer::Near => &NEAR,
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn alpha(self) -> f32 {
        self.spec().alpha
    }

    pub fn radius_range(self) -> (f32, f32) {
        self.spec().radius
    }

    /// Whether flakes of this layer land on piles and windows.
    /// Far flakes fall "behind" everything (PLAN.md §5).
    pub fn collides(self) -> bool {
        self != Layer::Far
    }

    fn pick(rng: &mut fastrand::Rng) -> Layer {
        let u = rng.f32();
        if u < FAR.share {
            Layer::Far
        } else if u < FAR.share + MID.share {
            Layer::Mid
        } else {
            Layer::Near
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flake {
    /// Centre, in global coordinates.
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub layer: Layer,
    fall: f32,
    sway_amp: f32,
    sway_freq: f32,
    sway_phase: f32,
}

fn lerp_range(rng: &mut fastrand::Rng, (lo, hi): (f32, f32)) -> f32 {
    lo + (hi - lo) * rng.f32()
}

/// `sin(freq * t + phase)`, keeping precision for large `t`.
fn phase_sin(t: f64, freq: f32, phase: f32) -> f32 {
    ((f64::from(freq) * t + f64::from(phase)) % TAU).sin() as f32
}

impl Flake {
    fn random(rng: &mut fastrand::Rng, bounds: &Rect, y: f32) -> Flake {
        let layer = Layer::pick(rng);
        let spec = layer.spec();
        Flake {
            x: bounds.x as f32 + rng.f32() * bounds.w as f32,
            y,
            r: lerp_range(rng, spec.radius),
            layer,
            fall: lerp_range(rng, spec.fall),
            sway_amp: lerp_range(rng, spec.sway),
            sway_freq: lerp_range(rng, (0.6, 1.8)),
            sway_phase: rng.f32() * TAU as f32,
        }
    }

    /// A fresh flake just above the top edge.
    fn spawn_top(rng: &mut fastrand::Rng, bounds: &Rect) -> Flake {
        let mut f = Flake::random(rng, bounds, 0.0);
        f.y = bounds.y as f32 - f.r - rng.f32() * SPAWN_HEIGHT;
        f
    }

    /// A fresh flake anywhere on screen (initial fill).
    fn spawn_anywhere(rng: &mut fastrand::Rng, bounds: &Rect) -> Flake {
        let y = bounds.y as f32 + rng.f32() * bounds.h as f32;
        Flake::random(rng, bounds, y)
    }
}

/// All falling flakes.
#[derive(Debug, Clone)]
pub struct Flakes {
    flakes: Vec<Flake>,
    rng: fastrand::Rng,
}

impl Flakes {
    pub fn new(seed: u64) -> Self {
        Self {
            flakes: Vec::new(),
            rng: fastrand::Rng::with_seed(seed),
        }
    }

    pub fn len(&self) -> usize {
        self.flakes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.flakes.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Flake> {
        self.flakes.iter()
    }

    /// Replace all flakes with `count` new ones spread over the whole area.
    pub fn scatter(&mut self, count: usize, bounds: &Rect) {
        let rng = &mut self.rng;
        self.flakes = (0..count)
            .map(|_| Flake::spawn_anywhere(rng, bounds))
            .collect();
    }

    /// Grow (new flakes enter from the top) or shrink to `count`.
    pub fn set_count(&mut self, count: usize, bounds: &Rect) {
        if count <= self.flakes.len() {
            self.flakes.truncate(count);
            return;
        }
        let rng = &mut self.rng;
        let extra = count - self.flakes.len();
        self.flakes
            .extend((0..extra).map(|_| Flake::spawn_top(rng, bounds)));
    }

    /// Advance every flake by `dt` seconds. `wind` is in px/s at full strength.
    pub fn step(&mut self, dt: f32, t: f64, wind: f32, fall_speed: f32, bounds: &Rect) {
        self.step_landing(dt, t, wind, fall_speed, bounds, |_, _| false);
    }

    /// Like [`step`](Self::step), but `land(flake, y_before)` may claim a
    /// flake (it settled on a pile); claimed flakes respawn at the top.
    pub fn step_landing(
        &mut self,
        dt: f32,
        t: f64,
        wind: f32,
        fall_speed: f32,
        bounds: &Rect,
        mut land: impl FnMut(&Flake, f32) -> bool,
    ) {
        let left = bounds.x as f32 - WRAP_MARGIN;
        let width = bounds.w as f32 + 2.0 * WRAP_MARGIN;
        let bottom = bounds.bottom() as f32;
        let rng = &mut self.rng;
        for f in &mut self.flakes {
            let sway = f.sway_amp * phase_sin(t, f.sway_freq, f.sway_phase);
            let y_before = f.y;
            f.x += (wind * f.layer.spec().wind + sway) * dt;
            f.y += f.fall * fall_speed * dt;
            f.x = wrap(f.x, left, width);
            if f.y - f.r > bottom || land(f, y_before) {
                *f = Flake::spawn_top(rng, bounds);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn as_mut_slice(&mut self) -> &mut [Flake] {
        &mut self.flakes
    }
}

/// Wrap `x` into `[left, left + width)`.
fn wrap(x: f32, left: f32, width: f32) -> f32 {
    let w = left + (x - left).rem_euclid(width);
    // rem_euclid can round up to exactly `width` for tiny negative offsets.
    if w >= left + width {
        left
    } else {
        w
    }
}

#[derive(Debug, Clone, Copy)]
struct Gust {
    start: f64,
    duration: f64,
    /// Signed peak strength in px/s.
    peak: f32,
}

/// Horizontal wind: two slow sine waves plus occasional eased gusts.
#[derive(Debug, Clone)]
pub struct Wind {
    rng: fastrand::Rng,
    strength: f32,
    gust: Option<Gust>,
    next_gust_at: f64,
    current: f32,
}

/// Mean drift to the right, px/s.
const BASE_DRIFT: f32 = 12.0;

impl Wind {
    pub fn new(seed: u64, strength: f32) -> Self {
        let mut rng = fastrand::Rng::with_seed(seed);
        let next_gust_at = 2.0 + rng.f64() * 6.0;
        Self {
            rng,
            strength,
            gust: None,
            next_gust_at,
            current: 0.0,
        }
    }

    pub fn set_strength(&mut self, strength: f32) {
        self.strength = strength;
    }

    /// Current wind in px/s (positive = to the right).
    pub fn current(&self) -> f32 {
        self.current
    }

    pub fn is_gusting(&self) -> bool {
        self.gust.is_some()
    }

    /// Recompute the wind for absolute sim time `t` (seconds).
    pub fn update(&mut self, t: f64) {
        let base = BASE_DRIFT + 18.0 * phase_sin(t, 0.11, 0.0) + 10.0 * phase_sin(t, 0.29, 1.7);
        self.current = self.strength * (base + self.gust_value(t));
    }

    fn gust_value(&mut self, t: f64) -> f32 {
        if self.gust.is_none() && t >= self.next_gust_at {
            self.gust = Some(self.new_gust(t));
        }
        let Some(g) = self.gust else { return 0.0 };
        let u = (t - g.start) / g.duration;
        if u >= 1.0 {
            self.gust = None;
            self.next_gust_at = t + 6.0 + self.rng.f64() * 9.0;
            return 0.0;
        }
        // sin² gives a smooth ease-in and ease-out.
        g.peak * ((PI * u).sin().powi(2) as f32)
    }

    fn new_gust(&mut self, start: f64) -> Gust {
        let sign = if self.rng.f32() < 0.75 { 1.0 } else { -1.0 };
        Gust {
            start,
            duration: 2.0 + self.rng.f64() * 3.0,
            peak: sign * (40.0 + self.rng.f32() * 70.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Rect = Rect::new(-300, 200, 800, 600);

    #[test]
    fn wrap_stays_in_range() {
        for x in [-1000.0, -0.000_01, 0.0, 99.999, 100.0, 1e6] {
            let w = wrap(x, 0.0, 100.0);
            assert!((0.0..100.0).contains(&w), "{x} -> {w}");
        }
    }

    #[test]
    fn flakes_stay_within_bounds_under_strong_wind() {
        let mut flakes = Flakes::new(3);
        flakes.scatter(500, &B);
        let mut wind = Wind::new(4, 5.0);
        let (left, right) = (B.x as f32 - WRAP_MARGIN, B.right() as f32 + WRAP_MARGIN);
        let top = B.y as f32 - SPAWN_HEIGHT - 4.0;
        let mut t = 0.0;
        for _ in 0..1200 {
            t += 1.0 / 60.0;
            wind.update(t);
            flakes.step(1.0 / 60.0, t, wind.current(), 1.0, &B);
            for f in flakes.iter() {
                assert!(f.x >= left && f.x < right, "x out of bounds: {f:?}");
                assert!(f.y >= top && f.y - f.r <= B.bottom() as f32, "y: {f:?}");
            }
        }
        assert_eq!(flakes.len(), 500);
    }

    #[test]
    fn flake_below_bottom_respawns_above_top() {
        let mut flakes = Flakes::new(1);
        flakes.scatter(10, &B);
        flakes.as_mut_slice()[0].y = B.bottom() as f32 + 50.0;
        flakes.step(1.0 / 60.0, 0.0, 0.0, 1.0, &B);
        let f = flakes.iter().next().unwrap();
        assert!(f.y < B.y as f32, "expected respawn above top, got {f:?}");
    }

    #[test]
    fn layers_are_distinct() {
        let mut flakes = Flakes::new(9);
        flakes.scatter(3000, &B);
        for layer in Layer::ALL {
            let (lo, hi) = layer.radius_range();
            let n = flakes.iter().filter(|f| f.layer == layer).count();
            assert!(n > 300, "{layer:?} has only {n} flakes");
            assert!(flakes
                .iter()
                .filter(|f| f.layer == layer)
                .all(|f| (lo..=hi).contains(&f.r)));
        }
        assert!(Layer::Near.alpha() > Layer::Mid.alpha());
        assert!(Layer::Mid.alpha() > Layer::Far.alpha());
        assert!(NEAR.fall.0 > MID.fall.1 && MID.fall.0 > FAR.fall.1);
        assert!(!Layer::Far.collides() && Layer::Near.collides());
    }

    #[test]
    fn set_count_grows_and_shrinks() {
        let mut flakes = Flakes::new(2);
        flakes.scatter(100, &B);
        flakes.set_count(150, &B);
        assert_eq!(flakes.len(), 150);
        assert!(flakes.iter().skip(100).all(|f| f.y < B.y as f32));
        flakes.set_count(20, &B);
        assert_eq!(flakes.len(), 20);
    }

    #[test]
    fn wind_drifts_right_on_average_and_gusts() {
        let mut wind = Wind::new(5, 1.0);
        let (mut sum, mut gusted) = (0.0, false);
        let steps = 60 * 120;
        for i in 0..steps {
            wind.update(f64::from(i) / 60.0);
            sum += wind.current();
            gusted |= wind.is_gusting();
        }
        assert!(sum / steps as f32 > 3.0, "mean wind {}", sum / steps as f32);
        assert!(gusted, "no gust in two minutes");
    }

    #[test]
    fn zero_strength_means_no_wind() {
        let mut wind = Wind::new(5, 0.0);
        for i in 0..600 {
            wind.update(f64::from(i) / 60.0);
            assert_eq!(wind.current(), 0.0);
        }
    }
}
