//! Frost (feature 4): after a while without mouse movement, frost creeps in
//! from each monitor's corners. Moving the mouse wipes a clear path through
//! it, like a fogged window; wiped spots grow back once you're idle again,
//! and during long active use the frost fades.

use crate::geom::{CursorState, OutputInfo, Rect};

/// Cell size (px). Frost is drawn as one crystal sprite per cell.
pub const CELL: i32 = 12;
/// Density levels drawn (density is quantised to these).
pub const LEVELS: u8 = 4;
/// Growth ticks per second.
const TICK: f32 = 0.25;
/// How far frost may reach from a corner, as a share of the shorter side,
/// at most, and how fast that reach grows once frosting (share per second).
const MAX_REACH: f32 = 0.42;
const REACH_GROWTH: f32 = 0.004;
/// Chance per tick that a cell next to frost freezes a bit more.
const GROW_CHANCE: f32 = 0.5;
/// The wipe radius around the pointer (px).
const WIPE_RADIUS: f32 = 30.0;
/// Wiped cells stay clear at least this long (s).
const WIPE_HOLD: f32 = 20.0;
/// While the pointer was idle this long, wiped spots may refreeze (s).
const REFREEZE_IDLE: f32 = 10.0;
/// After this long of continuous activity, frost fades (s), at this rate
/// (density per second).
const FADE_AFTER: f32 = 180.0;
const FADE_RATE: f32 = 0.05;

/// Frost on one monitor.
#[derive(Debug, Clone)]
struct Pane {
    rect: Rect,
    cols: usize,
    rows: usize,
    /// 0..=1 per cell.
    density: Vec<f32>,
    /// When each cell was last wiped (frost time, s; stays clear for WIPE_HOLD).
    wiped_at: Vec<f32>,
    /// Per-cell randomness: makes the front ragged and branchy.
    affinity: Vec<f32>,
    /// Normalised distance of each cell from its nearest corner.
    corner_dist: Vec<f32>,
    /// Needle direction (0..ORIENTATIONS): away from the corner, jittered,
    /// so neighbouring cells line up into feathery streaks.
    orient: Vec<u8>,
}

/// Needle directions over 180°.
pub const ORIENTATIONS: u8 = 8;

impl Pane {
    fn new(rect: Rect, rng: &mut fastrand::Rng) -> Pane {
        let cols = rect.w.div_ceil(CELL as u32).max(1) as usize;
        let rows = rect.h.div_ceil(CELL as u32).max(1) as usize;
        let short = rect.w.min(rect.h) as f32;
        let (w, h) = (rect.w as f32, rect.h as f32);
        let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)];
        let mut corner_dist = Vec::with_capacity(rows * cols);
        let mut orient = Vec::with_capacity(rows * cols);
        for i in 0..rows * cols {
            let (cx, cy) = (
                ((i % cols) as i32 * CELL + CELL / 2) as f32,
                ((i / cols) as i32 * CELL + CELL / 2) as f32,
            );
            let (d, (kx, ky)) = corners
                .iter()
                .map(|&(x, y)| ((cx - x).hypot(cy - y), (x, y)))
                .fold((f32::MAX, (0.0, 0.0)), |a, b| if b.0 < a.0 { b } else { a });
            corner_dist.push(d / short);
            let angle = (cy - ky).atan2(cx - kx) + (rng.f32() - 0.5) * 0.5;
            let a = angle.rem_euclid(std::f32::consts::PI) / std::f32::consts::PI;
            orient.push((a * ORIENTATIONS as f32) as u8 % ORIENTATIONS);
        }
        Pane {
            rect,
            cols,
            rows,
            density: vec![0.0; rows * cols],
            wiped_at: vec![f32::MIN; rows * cols],
            affinity: (0..rows * cols).map(|_| 0.15 + rng.f32() * 0.85).collect(),
            corner_dist,
            orient,
        }
    }

    fn neighbours(&self, i: usize) -> f32 {
        let (c, r) = ((i % self.cols) as i64, (i / self.cols) as i64);
        let mut sum = 0.0;
        for (dc, dr) in [
            (-1, 0),
            (1, 0),
            (0, -1),
            (0, 1),
            (-1, -1),
            (1, 1),
            (-1, 1),
            (1, -1),
        ] {
            let (nc, nr) = (c + dc, r + dr);
            if nc >= 0 && nr >= 0 && (nc as usize) < self.cols && (nr as usize) < self.rows {
                sum += self.density[nr as usize * self.cols + nc as usize];
            }
        }
        sum
    }

    fn grow(&mut self, reach: f32, now: f32, rng: &mut fastrand::Rng) {
        let old = self.density.clone();
        for i in 0..old.len() {
            let held = now - self.wiped_at[i] < WIPE_HOLD;
            if self.corner_dist[i] > reach || held || old[i] >= 1.0 {
                continue;
            }
            let at_corner = self.corner_dist[i] < 0.02;
            let pull = if at_corner {
                1.0
            } else {
                (self.neighbours(i) / 2.5).min(1.0)
            };
            // Thinner towards the edge of the reach: feathery fronts.
            let edge = 1.0 - self.corner_dist[i] / reach.max(1e-3);
            // Thick near the corner, thinning out towards the edge.
            let cap = (edge * 1.4 + 0.15).clamp(0.25, 1.0);
            if rng.f32() < GROW_CHANCE * pull * self.affinity[i] * (0.3 + edge) {
                self.density[i] = (old[i] + 1.0 / LEVELS as f32).min(cap);
            }
        }
    }

    /// Clear the cells along the pointer's path from `a` to `b` (local px).
    fn wipe(&mut self, a: (f32, f32), b: (f32, f32), now: f32) {
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        let steps = (len / (WIPE_RADIUS * 0.5)).ceil().max(1.0) as usize;
        let r = (WIPE_RADIUS / CELL as f32).ceil() as i64 + 1;
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let (x, y) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            let (cc, cr) = ((x / CELL as f32) as i64, (y / CELL as f32) as i64);
            for row in cr - r..=cr + r {
                for col in cc - r..=cc + r {
                    if row < 0 || col < 0 || row as usize >= self.rows || col as usize >= self.cols
                    {
                        continue;
                    }
                    let (mx, my) = (
                        (col as i32 * CELL + CELL / 2) as f32,
                        (row as i32 * CELL + CELL / 2) as f32,
                    );
                    if (mx - x).hypot(my - y) <= WIPE_RADIUS {
                        let i = row as usize * self.cols + col as usize;
                        if self.density[i] > 0.0 {
                            self.density[i] = 0.0;
                            self.wiped_at[i] = now;
                        }
                    }
                }
            }
        }
    }
}

/// One frosted cell to draw: global top-left, level 1..=LEVELS, and its
/// look: needle direction × 2 + branch pattern (0..2 × ORIENTATIONS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrostCell {
    pub x: i32,
    pub y: i32,
    pub level: u8,
    pub variant: u8,
}

#[derive(Debug, Clone)]
pub struct Frost {
    panes: Vec<Pane>,
    idle: f32,
    active: f32,
    reach: f32,
    /// Time since the last growth tick, and total frost time.
    tick: f32,
    time: f32,
    /// Some cell may be frosted (cheap check before touching every cell).
    any: bool,
    /// Bumped whenever a cell may have changed, so drawing can skip
    /// unchanged frames.
    version: u64,
    last_cursor: Option<(f32, f32)>,
    rng: fastrand::Rng,
}

impl Default for Frost {
    fn default() -> Self {
        Self {
            panes: Vec::new(),
            idle: 0.0,
            active: 0.0,
            reach: 0.0,
            tick: 0.0,
            time: 0.0,
            any: false,
            version: 0,
            last_cursor: None,
            rng: fastrand::Rng::with_seed(0x0F20_57ED),
        }
    }
}

impl Frost {
    pub fn clear(&mut self) {
        if self.any {
            self.version += 1;
            for p in &mut self.panes {
                p.density.fill(0.0);
            }
            self.any = false;
        }
        self.reach = 0.0;
    }

    /// Changes whenever [`cells`](Self::cells) may have changed.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Frosted cells (density quantised to levels), global coordinates.
    pub fn cells(&self) -> impl Iterator<Item = FrostCell> + '_ {
        let panes = if self.any { &self.panes[..] } else { &[] };
        panes.iter().flat_map(|p| {
            p.density.iter().enumerate().filter_map(move |(i, &d)| {
                let level = (d * LEVELS as f32).round() as u8;
                (level > 0).then(|| FrostCell {
                    x: p.rect.x + (i % p.cols) as i32 * CELL,
                    y: p.rect.y + (i / p.cols) as i32 * CELL,
                    level: level.min(LEVELS),
                    // Two branch patterns per direction, picked per cell.
                    variant: p.orient[i] * 2 + u8::from(p.affinity[i] > 0.55),
                })
            })
        })
    }

    /// `idle_after`: seconds without pointer movement before frost starts.
    /// Without a cursor (unknown pointer) frost stays off.
    pub fn step(
        &mut self,
        dt: f32,
        outputs: &[OutputInfo],
        cursor: Option<CursorState>,
        idle_after: f32,
    ) {
        let rects: Vec<Rect> = outputs.iter().map(|o| o.rect).collect();
        if self.panes.iter().map(|p| p.rect).ne(rects.iter().copied()) {
            self.panes = rects.iter().map(|r| Pane::new(*r, &mut self.rng)).collect();
            self.version += 1;
        }
        let Some(c) = cursor else {
            self.clear();
            return;
        };
        let moved = self.last_cursor.is_some_and(|(x, y)| (x, y) != (c.x, c.y));
        if moved {
            if self.any {
                self.version += 1;
            }
            let from = self.last_cursor.unwrap_or((c.x, c.y));
            let now = self.time;
            for p in &mut self.panes {
                let (ox, oy) = (p.rect.x as f32, p.rect.y as f32);
                p.wipe((from.0 - ox, from.1 - oy), (c.x - ox, c.y - oy), now);
            }
            self.idle = 0.0;
            self.active += dt;
        } else {
            self.idle += dt;
            if self.idle > 2.0 {
                self.active = 0.0;
            }
        }
        self.last_cursor = Some((c.x, c.y));
        self.time += dt;
        self.tick += dt;
        // Everything below touches every cell: only a few times a second.
        if self.tick < TICK {
            return;
        }
        let elapsed = std::mem::take(&mut self.tick);
        if self.any {
            self.version += 1;
        }
        let frosting = self.idle >= idle_after;
        if frosting {
            self.reach = (self.reach + REACH_GROWTH * elapsed).min(MAX_REACH);
        }
        let has_frost = self.any
            && self
                .panes
                .iter()
                .any(|p| p.density.iter().any(|&d| d > 0.0));
        self.any = has_frost;
        if self.active > FADE_AFTER {
            for p in &mut self.panes {
                for d in &mut p.density {
                    *d = (*d - FADE_RATE * elapsed).max(0.0);
                }
            }
            if !has_frost {
                self.reach = 0.0;
            }
        }
        if frosting || (has_frost && self.idle >= REFREEZE_IDLE) {
            let reach = if frosting {
                self.reach
            } else {
                self.reach.max(0.05)
            };
            let now = self.time;
            for p in &mut self.panes {
                p.grow(reach, now, &mut self.rng);
            }
            self.any = true;
            self.version += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out() -> Vec<OutputInfo> {
        vec![OutputInfo {
            name: "A".into(),
            rect: Rect::new(0, 0, 1920, 1080),
            scale: 1.0,
        }]
    }

    fn at(x: f32, y: f32) -> Option<CursorState> {
        Some(CursorState {
            x,
            y,
            ..CursorState::default()
        })
    }

    fn run(f: &mut Frost, secs: f32, cursor: impl Fn(f32) -> Option<CursorState>) {
        let mut t = 0.0;
        while t < secs {
            f.step(1.0 / 30.0, &out(), cursor(t), 60.0);
            t += 1.0 / 30.0;
        }
    }

    #[test]
    fn frost_grows_in_the_corners_when_idle() {
        let mut f = Frost::default();
        run(&mut f, 50.0, |_| at(960.0, 540.0));
        assert_eq!(f.cells().count(), 0, "not idle long enough");
        run(&mut f, 120.0, |_| at(960.0, 540.0));
        let cells: Vec<_> = f.cells().collect();
        assert!(cells.len() > 100, "{} frosted cells", cells.len());
        let centre = cells
            .iter()
            .any(|c| (c.x - 960).abs() < 200 && (c.y - 540).abs() < 200);
        assert!(!centre, "frost stays near the corners");
        assert!(
            cells.iter().any(|c| c.x < 100 && c.y < 100),
            "top-left corner"
        );
        assert!(
            cells.iter().any(|c| c.x > 1800 && c.y > 960),
            "bottom-right corner"
        );
    }

    #[test]
    fn moving_the_mouse_wipes_a_path() {
        let mut f = Frost::default();
        run(&mut f, 200.0, |_| at(960.0, 540.0));
        // Cells along the top-left diagonal.
        let band = |f: &Frost| {
            f.cells()
                .filter(|c| c.x < 120 && c.y < 120 && (c.x - c.y).abs() < 12)
                .count()
        };
        assert!(band(&f) > 3, "{}", band(&f));
        // Swipe along that diagonal.
        run(&mut f, 1.0, |t| at(5.0 + t * 150.0, 5.0 + t * 150.0));
        assert_eq!(band(&f), 0, "the swipe left a clear path");
    }

    #[test]
    fn no_cursor_no_frost() {
        let mut f = Frost::default();
        run(&mut f, 300.0, |_| None);
        assert_eq!(f.cells().count(), 0);
    }
}
