//! Snowballs and snowmen. Needs the cursor (X11).
//!
//! - Moving the cursor sideways through a pile scoops up a small snowball
//!   and pushes it; rolling over snow makes it grow.
//! - Snowballs roll with friction, ride on moving windows, fall off edges,
//!   push each other, and can rest on top of each other.
//! - A ball that falls a long way **poofs** into the pile it lands on.
//! - Hovering over the upper half of a ball makes it clickable (no
//!   modifier needed): left-drag picks it up, release drops (or throws) it.
//!   Pushing only happens lower down, at snow level, so reaching for a ball
//!   doesn't roll it away. With the carry modifier held, the whole ball is
//!   clickable.
//! - Any three balls stacked on each other become a snowman; their snow
//!   rearranges so the biggest ball is at the bottom.

use std::f32::consts::PI;

use crate::config::CarryModifier;
use crate::geom::{CursorState, Rect};
use crate::sim::particles::{Particle, Particles};
use crate::sim::piles::{Piles, SurfaceKey};

const GRAVITY: f32 = 900.0;
/// Fraction of rolling speed lost per second.
const ROLL_FRICTION: f32 = 1.4;
/// Air drag on free balls, per second.
const AIR_DRAG: f32 = 0.3;
/// Snow picked up while rolling: px of height per px rolled, under the ball.
const PICK_PER_PX: f32 = 0.06;
/// The cursor must move at least this fast (px/s) to push or scoop.
const MIN_PUSH_SPEED: f32 = 40.0;
const MAX_PUSH_SPEED: f32 = 900.0;
/// Falling further than this (px) makes a ball poof on landing.
const POOF_FALL: f32 = 160.0;
const MIN_RADIUS: f32 = 2.5;
pub const MAX_RADIUS: f32 = 42.0;
/// Snow needed to start a ball (px²).
const START_VOLUME: f32 = 10.0;
const MAX_BALLS: usize = 24;
/// Balls must rest this long (s) before three of them become a snowman.
const SNOWMAN_SETTLE: f32 = 0.4;
/// Each ball of a snowman must be at least this much bigger than the next.
const SNOWMAN_RATIO: f32 = 1.1;
/// A ball resting on another stays on if its centre is within this share
/// of the lower ball's radius; further out it slides off.
const BALANCE: f32 = 0.45;
/// Extra px around a ball that still counts as on it.
const GRAB_MARGIN: f32 = 3.0;
/// Cursor samples older than this count as "not moving".
const CURSOR_STALE: f64 = 0.12;

#[derive(Debug, Clone, PartialEq)]
enum State {
    Free {
        fall_from: f32,
    },
    /// Resting/rolling on a pile; `x = surface.x0 + offset`.
    OnSurface {
        key: SurfaceKey,
        offset: f32,
    },
    /// Sitting on ball `id`; `x = that.x + offset`.
    OnBall {
        id: u32,
        offset: f32,
    },
    /// Held by the cursor at this offset from it.
    Carried {
        dx: f32,
        dy: f32,
    },
}

#[derive(Debug, Clone)]
pub struct Snowball {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub vx: f32,
    pub vy: f32,
    /// Snow in the ball (px²); sets its size.
    pub volume: f32,
    state: State,
    /// How long it has been at rest (s).
    settled: f32,
}

impl Snowball {
    pub fn radius(&self) -> f32 {
        radius_of(self.volume)
    }

    pub fn is_carried(&self) -> bool {
        matches!(self.state, State::Carried { .. })
    }

    fn resting(&self) -> bool {
        matches!(self.state, State::OnSurface { .. } | State::OnBall { .. })
    }
}

/// The snow of three stacked balls, rearranged bottom to top so each ball
/// is clearly bigger than the one above it (`SNOWMAN_RATIO` in radius).
/// Total snow stays the same.
fn snowman_volumes(mut v: [f32; 3]) -> [f32; 3] {
    v.sort_by(|a, b| b.total_cmp(a));
    let total: f32 = v.iter().sum();
    // Radius ratio k means volume ratio k².
    let k2 = SNOWMAN_RATIO * SNOWMAN_RATIO * 1.05;
    let top = v[2].min(total / (1.0 + k2 + k2 * k2));
    let mid = v[1].max(top * k2).min((total - top) / (1.0 + k2));
    [total - mid - top, mid, top]
}

pub fn radius_of(volume: f32) -> f32 {
    (volume / PI).sqrt().clamp(MIN_RADIUS, MAX_RADIUS)
}

/// A finished snowman, standing on a surface.
#[derive(Debug, Clone)]
pub struct Snowman {
    /// Centre x and the y of its base (bottom of the lowest ball).
    pub x: f32,
    pub base: f32,
    /// Ball radii, bottom to top.
    pub radii: [f32; 3],
    key: SurfaceKey,
    offset: f32,
    volumes: [f32; 3],
}

impl Snowman {
    /// Stacking depth of what it stands on (like `Surface::z`).
    pub fn depth(&self, piles: &Piles) -> i32 {
        piles.surface(&self.key).map_or(i32::MAX, |s| s.z)
    }

    /// Bounding box (global), including arms and hat room.
    pub fn bounds(&self) -> Rect {
        let [b, m, t] = self.radii;
        let h = 2.0 * (b + m + t) + t * 1.2;
        let w = 2.0 * b.max(m * 2.4);
        Rect::new(
            (self.x - w / 2.0).floor() as i32,
            (self.base - h).floor() as i32,
            w.ceil() as u32 + 1,
            h.ceil() as u32 + 1,
        )
    }
}

/// All snowballs and snowmen, plus the cursor's recent motion.
#[derive(Debug, Clone, Default)]
pub struct Snowfolk {
    balls: Vec<Snowball>,
    men: Vec<Snowman>,
    next_id: u32,
    /// Last cursor sample that differed from the one before, and when.
    cursor: Option<(CursorState, f64)>,
    cursor_vel: (f32, f32),
    left_was_down: bool,
}

/// What `Snowfolk::step` needs from the rest of the world.
pub struct Env<'a> {
    pub dt: f32,
    pub now: f64,
    pub cursor: Option<CursorState>,
    pub carry: CarryModifier,
    pub piles: &'a mut Piles,
    pub particles: &'a mut Particles,
    pub bounds: Rect,
    pub max_pile_height: f32,
    /// Windows and panels with their stacking depth (see `World::occluders`).
    pub occluders: &'a [(i32, Rect)],
}

impl Env<'_> {
    /// Is (`x`, `y`) on something at depth `z` hidden by a window above it?
    pub fn covered(&self, x: f32, y: f32, z: i32) -> bool {
        self.occluders
            .iter()
            .any(|(oz, r)| *oz > z && r.contains(x as i32, y as i32))
    }
}

pub fn modifier_held(carry: CarryModifier, c: &CursorState) -> bool {
    match carry {
        CarryModifier::None => true,
        CarryModifier::Alt => c.alt,
        CarryModifier::Super => c.super_,
    }
}

impl Snowfolk {
    pub fn balls(&self) -> &[Snowball] {
        &self.balls
    }

    pub fn snowmen(&self) -> &[Snowman] {
        &self.men
    }

    pub fn clear(&mut self) {
        self.balls.clear();
        self.men.clear();
    }

    /// Stacking depth of ball `i`: that of the surface it rests on (through
    /// any balls below it), or on top of everything while it flies or is
    /// carried. Windows stacked above that depth hide it.
    pub fn ball_depth(&self, i: usize, piles: &Piles) -> i32 {
        let mut b = &self.balls[i];
        for _ in 0..self.balls.len() {
            match &b.state {
                State::OnSurface { key, .. } => {
                    return piles.surface(key).map_or(i32::MAX, |s| s.z);
                }
                State::OnBall { id, .. } => match self.balls.iter().find(|o| o.id == *id) {
                    Some(o) => b = o,
                    None => return i32::MAX,
                },
                State::Free { .. } | State::Carried { .. } => return i32::MAX,
            }
        }
        i32::MAX
    }

    /// Throw resting balls and snowmen where `hit(x, y)` gives a velocity
    /// (the tornado). Snowmen fall apart into their three balls.
    pub fn blow(&mut self, mut hit: impl FnMut(f32, f32) -> Option<(f32, f32)>) {
        for b in &mut self.balls {
            if !b.resting() {
                continue;
            }
            if let Some(v) = hit(b.x, b.y) {
                b.state = State::Free { fall_from: b.y };
                (b.vx, b.vy) = v;
            }
        }
        let mut hits = Vec::new();
        let mut kept = Vec::new();
        for m in self.men.drain(..) {
            let mid_y = m.base - m.radii[0];
            match hit(m.x, mid_y) {
                Some(v) => hits.push((m, v)),
                None => kept.push(m),
            }
        }
        self.men = kept;
        for (m, v) in hits {
            self.break_snowman(&m, v);
        }
    }

    pub fn is_carrying(&self) -> bool {
        self.balls.iter().any(Snowball::is_carried)
    }

    /// Where clicks should be caught instead of passed through: the upper
    /// half of every ball (to pick it up), and whole balls while the carry
    /// modifier is held or a ball is being carried.
    pub fn grab_rects(&self, cursor: Option<&CursorState>, carry: CarryModifier) -> Vec<Rect> {
        let whole = self.is_carrying() || cursor.is_some_and(|c| modifier_held(carry, c));
        self.balls
            .iter()
            .map(|b| {
                let r = b.radius() + GRAB_MARGIN;
                let h = if whole { 2.0 * r } else { r };
                Rect::new(
                    (b.x - r).floor() as i32,
                    (b.y - r).floor() as i32,
                    (2.0 * r).ceil() as u32,
                    h.ceil() as u32,
                )
            })
            .collect()
    }

    pub fn step(&mut self, env: &mut Env) {
        self.track_cursor(env);
        self.knock_off(env);
        if let Some(c) = env.cursor {
            self.interact(&c, env);
        }
        for i in 0..self.balls.len() {
            self.move_ball(i, env);
        }
        self.collide();
        self.build_snowmen(env);
        self.move_snowmen(env);
        let b = env.bounds;
        self.balls.retain(|ball| {
            ball.y < (b.bottom() + 100) as f32
                && ball.x > (b.x - 100) as f32
                && ball.x < (b.right() + 100) as f32
        });
    }

    fn track_cursor(&mut self, env: &Env) {
        let Some(c) = env.cursor else {
            self.cursor = None;
            self.cursor_vel = (0.0, 0.0);
            return;
        };
        match self.cursor {
            Some((prev, t)) if (prev.x, prev.y) != (c.x, c.y) => {
                let dt = ((env.now - t) as f32).max(1.0 / 120.0);
                self.cursor_vel = ((c.x - prev.x) / dt, (c.y - prev.y) / dt);
                self.cursor = Some((c, env.now));
            }
            Some((_, t)) => {
                if env.now - t > CURSOR_STALE {
                    self.cursor_vel = (0.0, 0.0);
                }
                // Keep the old position/time, but the latest buttons.
                self.cursor = self.cursor.map(|(p, t)| {
                    (
                        CursorState {
                            x: p.x,
                            y: p.y,
                            ..c
                        },
                        t,
                    )
                });
            }
            None => self.cursor = Some((c, env.now)),
        }
    }

    /// Balls and snowmen on a window that was flung fly off with it.
    fn knock_off(&mut self, env: &mut Env) {
        for (key, v) in env.piles.knocked().to_vec() {
            for b in &mut self.balls {
                if matches!(&b.state, State::OnSurface { key: k, .. } if *k == key) {
                    b.state = State::Free { fall_from: b.y };
                    (b.vx, b.vy) = v;
                }
            }
            let (gone, kept): (Vec<Snowman>, Vec<Snowman>) =
                self.men.drain(..).partition(|m| m.key == key);
            self.men = kept;
            for m in gone {
                self.break_snowman(&m, v);
            }
        }
    }

    /// Turn a snowman back into three free balls flying at `v`.
    fn break_snowman(&mut self, m: &Snowman, v: (f32, f32)) {
        let mut y = m.base;
        for (r, vol) in m.radii.into_iter().zip(m.volumes) {
            y -= r;
            let id = self.new_id();
            self.balls.push(Snowball {
                id,
                x: m.x,
                y,
                vx: v.0,
                vy: v.1,
                volume: vol,
                state: State::Free { fall_from: y },
                settled: 0.0,
            });
            y -= r;
        }
    }

    fn new_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    fn interact(&mut self, c: &CursorState, env: &mut Env) {
        let pressed = c.left_down && !self.left_was_down;
        self.left_was_down = c.left_down;
        let (vx, _) = self.cursor_vel;

        // Release: drop or throw what we carry.
        if !c.left_down {
            for b in &mut self.balls {
                if b.is_carried() {
                    b.state = State::Free { fall_from: b.y };
                    b.vx = self.cursor_vel.0.clamp(-1500.0, 1500.0);
                    b.vy = self.cursor_vel.1.clamp(-1500.0, 1500.0);
                }
            }
        }
        // Pick up the ball under the cursor: by its upper half, or anywhere
        // with the carry modifier.
        if pressed {
            let any_part = modifier_held(env.carry, c);
            let hit = self
                .balls
                .iter()
                .enumerate()
                .filter(|(i, b)| {
                    (b.x - c.x).hypot(b.y - c.y) <= b.radius() + GRAB_MARGIN
                        && (any_part || c.y <= b.y)
                        && !env.covered(c.x, c.y, self.ball_depth(*i, env.piles))
                })
                .min_by(|(_, a), (_, b)| a.radius().total_cmp(&b.radius()))
                .map(|(i, _)| i);
            if let Some(i) = hit {
                let id = self.balls[i].id;
                let (dx, dy) = (self.balls[i].x - c.x, self.balls[i].y - c.y);
                self.balls[i].state = State::Carried { dx, dy };
                // Anything sitting on it falls.
                for b in &mut self.balls {
                    if matches!(b.state, State::OnBall { id: s, .. } if s == id) {
                        b.state = State::Free { fall_from: b.y };
                    }
                }
                return;
            }
        }
        // Modifier + left drag is for carrying or writing in the snow,
        // not for rolling.
        let writing = c.left_down && modifier_held(env.carry, c);
        if self.is_carrying() || writing || vx.abs() < MIN_PUSH_SPEED {
            return;
        }
        // Push a ball the cursor runs into: it stays just ahead of the
        // pointer, so a sweep keeps rolling the same ball.
        let push = vx.clamp(-MAX_PUSH_SPEED, MAX_PUSH_SPEED);
        let dir = vx.signum();
        let depths: Vec<i32> = (0..self.balls.len())
            .map(|i| self.ball_depth(i, env.piles))
            .collect();
        let hidden = |i: usize| env.covered(c.x, c.y, depths[i]);
        if let Some((_, b)) = self.balls.iter_mut().enumerate().find(|(i, b)| {
            let r = b.radius();
            if hidden(*i) {
                return false;
            }
            // From its middle down into the snow it sits on; the upper half
            // is for picking it up.
            (b.x - c.x).abs() <= r + GRAB_MARGIN && c.y > b.y && c.y <= b.y + r + 12.0
        }) {
            let r = b.radius();
            let ahead = c.x + dir * r * 0.9;
            if (ahead - b.x) * dir > 0.0 {
                let dx = ahead - b.x;
                b.x = ahead;
                if let State::OnSurface { offset, .. } = &mut b.state {
                    *offset += dx;
                }
            }
            if push.abs() > b.vx.abs() || push.signum() != b.vx.signum() {
                b.vx = push;
            }
            return;
        }
        // Scoop a new ball out of the snow the cursor is moving through.
        if self.balls.len() >= MAX_BALLS
            || self
                .balls
                .iter()
                .any(|b| (b.x - c.x).abs() < b.radius() + 30.0 && (b.y - c.y).abs() < 60.0)
        {
            return;
        }
        let Some((key, _)) = env.piles.snow_at(c.x, c.y, 2.0) else {
            return;
        };
        let z = env.piles.surface(&key).map_or(i32::MAX, |s| s.z);
        if env.covered(c.x, c.y, z) {
            return;
        }
        let volume = env.piles.take(&key, c.x, 5.0, 4.0);
        if volume < START_VOLUME * 0.5 {
            return;
        }
        let volume = volume.max(START_VOLUME);
        let r = radius_of(volume);
        let x = c.x + vx.signum() * (r + 1.0);
        let Some(s) = env.piles.surface(&key) else {
            return;
        };
        let y = s.top_at(x).unwrap_or(s.y) - r;
        let id = self.new_id();
        self.balls.push(Snowball {
            id,
            x,
            y,
            vx: push,
            vy: 0.0,
            volume,
            state: State::OnSurface {
                offset: x - s.x0,
                key,
            },
            settled: 0.0,
        });
    }

    fn move_ball(&mut self, i: usize, env: &mut Env) {
        let dt = env.dt;
        let state = self.balls[i].state.clone();
        match state {
            State::Carried { dx, dy } => {
                if let Some((c, _)) = self.cursor {
                    let b = &mut self.balls[i];
                    (b.x, b.y) = (c.x + dx, c.y + dy);
                    (b.vx, b.vy) = self.cursor_vel;
                }
                self.balls[i].settled = 0.0;
            }
            State::OnSurface { key, offset } => self.roll_on_surface(i, &key, offset, env),
            State::OnBall { id, offset } => self.sit_on_ball(i, id, offset, dt),
            State::Free { fall_from } => self.fly(i, fall_from, env),
        }
    }

    fn roll_on_surface(&mut self, i: usize, key: &SurfaceKey, offset: f32, env: &mut Env) {
        let dt = env.dt;
        let Some(s) = env.piles.surface(key) else {
            // Its window closed or maximized.
            let b = &mut self.balls[i];
            b.state = State::Free { fall_from: b.y };
            return;
        };
        let b = &mut self.balls[i];
        let offset = offset + b.vx * dt;
        let x = s.x0 + offset;
        let r = b.radius();
        let Some(top) = s.top_at(x) else {
            // Rolled off the end.
            b.state = State::Free { fall_from: b.y };
            b.x = x;
            return;
        };
        b.x = x;
        b.y = top - r;
        b.vx *= (1.0 - ROLL_FRICTION * dt).max(0.0);
        if b.vx.abs() < 3.0 {
            b.vx = 0.0;
            b.settled += dt;
        } else {
            b.settled = 0.0;
        }
        b.state = State::OnSurface {
            key: key.clone(),
            offset,
        };
        // Rolling over snow makes it grow.
        let rolled = (b.vx * dt).abs();
        if rolled > 0.0 && r < MAX_RADIUS {
            let picked = env.piles.take(key, x, r * 0.5, PICK_PER_PX * rolled);
            self.balls[i].volume += picked;
        }
    }

    fn sit_on_ball(&mut self, i: usize, id: u32, offset: f32, dt: f32) {
        let support = self
            .balls
            .iter()
            .find(|b| b.id == id && !b.is_carried())
            .map(|b| (b.x, b.y, b.radius(), b.resting()));
        let b = &mut self.balls[i];
        let Some((sx, sy, sr, sup_resting)) = support else {
            b.state = State::Free { fall_from: b.y };
            return;
        };
        let r = b.radius();
        let mut offset = offset;
        if offset.abs() < BALANCE * sr {
            offset *= (1.0 - 3.0 * dt).max(0.0); // settles to the middle
        } else {
            offset += offset.signum() * 80.0 * dt; // slides off
        }
        b.x = sx + offset;
        b.y = sy - sr - r + (offset / sr).powi(2) * sr * 0.5;
        if offset.abs() > sr {
            b.vx = offset.signum() * 60.0;
            b.state = State::Free { fall_from: b.y };
            return;
        }
        b.settled = if sup_resting { b.settled + dt } else { 0.0 };
        b.state = State::OnBall { id, offset };
    }

    fn fly(&mut self, i: usize, fall_from: f32, env: &mut Env) {
        let dt = env.dt;
        let others: Vec<(u32, f32, f32, f32)> = self
            .balls
            .iter()
            .enumerate()
            .filter(|(j, o)| *j != i && o.resting())
            .map(|(_, o)| (o.id, o.x, o.y, o.radius()))
            .collect();
        let b = &mut self.balls[i];
        let r = b.radius();
        let y_before = b.y;
        b.vy += GRAVITY * dt;
        let drag = (1.0 - AIR_DRAG * dt).max(0.0);
        b.vx *= drag;
        b.x += b.vx * dt;
        b.y += b.vy * dt;
        b.settled = 0.0;
        let fall_from = fall_from.min(b.y);
        if b.vy <= 0.0 {
            b.state = State::Free { fall_from };
            return;
        }
        // Land on another ball?
        for (id, ox, oy, or) in others {
            let top = oy - or;
            if (b.x - ox).abs() < or * 0.9 && y_before + r <= top + 1.0 && b.y + r >= top {
                b.state = State::OnBall {
                    id,
                    offset: b.x - ox,
                };
                (b.vx, b.vy) = (0.0, 0.0);
                return;
            }
        }
        // Land on a pile?
        let Some((key, top)) = env.piles.landing(b.x, y_before + r, b.y + r) else {
            b.state = State::Free { fall_from };
            return;
        };
        if b.y - fall_from > POOF_FALL {
            // Poof: most of it joins the pile, a little puffs up.
            let (x, volume) = (b.x, b.volume);
            env.piles.land(
                x,
                top - r - 1.0,
                top - r + 1.0,
                r,
                volume * 0.85,
                env.max_pile_height,
            );
            for k in 0..6 {
                env.particles.spawn(
                    Particle {
                        x: x + (k as f32 - 2.5) * r * 0.3,
                        y: top - 2.0,
                        vx: (k as f32 - 2.5) * 30.0,
                        vy: -120.0,
                        volume: volume * 0.025,
                    },
                    25.0,
                );
            }
            b.volume = 0.0;
            return;
        }
        let Some(s) = env.piles.surface(&key) else {
            return;
        };
        b.y = top - r;
        b.vy = 0.0;
        b.state = State::OnSurface {
            offset: b.x - s.x0,
            key,
        };
    }

    /// Push overlapping balls apart (except a ball and the one it sits on).
    fn collide(&mut self) {
        self.balls.retain(|b| b.volume > 0.0);
        let n = self.balls.len();
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = (&self.balls[i], &self.balls[j]);
                let stacked = matches!(a.state, State::OnBall { id, .. } if id == b.id)
                    || matches!(b.state, State::OnBall { id, .. } if id == a.id);
                if stacked || a.is_carried() || b.is_carried() {
                    continue;
                }
                let (dx, dy) = (b.x - a.x, b.y - a.y);
                let d = dx.hypot(dy).max(0.01);
                let overlap = a.radius() + b.radius() - d;
                if overlap <= 0.0 {
                    continue;
                }
                // Mostly sideways: they roll along surfaces.
                let push = overlap * 0.5 * dx.signum();
                let (va, vb) = (a.vx, b.vx);
                let shove = |ball: &mut Snowball, dx: f32, v: f32| {
                    ball.x += dx;
                    if let State::OnSurface { offset, .. } | State::OnBall { offset, .. } =
                        &mut ball.state
                    {
                        *offset += dx;
                    }
                    ball.vx = v * 0.8;
                };
                shove(&mut self.balls[i], -push, vb);
                shove(&mut self.balls[j], push, va);
            }
        }
    }

    fn build_snowmen(&mut self, env: &Env) {
        let settled = |b: &Snowball| b.settled >= SNOWMAN_SETTLE && !b.is_carried();
        let on = |id: u32| {
            self.balls
                .iter()
                .position(|b| matches!(b.state, State::OnBall { id: s, .. } if s == id))
        };
        let mut found = None;
        for (bi, bottom) in self.balls.iter().enumerate() {
            let State::OnSurface { key, offset } = &bottom.state else {
                continue;
            };
            let Some(mi) = on(bottom.id) else { continue };
            let Some(ti) = on(self.balls[mi].id) else {
                continue;
            };
            let (b, m, t) = (bottom, &self.balls[mi], &self.balls[ti]);
            if settled(b) && settled(m) && settled(t) {
                found = Some((bi, mi, ti, key.clone(), *offset));
                break;
            }
        }
        let Some((bi, mi, ti, key, offset)) = found else {
            return;
        };
        let (b, m, t) = (&self.balls[bi], &self.balls[mi], &self.balls[ti]);
        let volumes = snowman_volumes([b.volume, m.volume, t.volume]);
        let man = Snowman {
            x: b.x,
            base: b.y + b.radius(),
            radii: volumes.map(radius_of),
            key,
            offset,
            volumes,
        };
        let ids = [b.id, m.id, t.id];
        self.balls.retain(|x| !ids.contains(&x.id));
        log::info!("a snowman was built");
        self.men.push(man);
        let _ = env;
    }

    fn move_snowmen(&mut self, env: &mut Env) {
        let mut broken = Vec::new();
        self.men.retain_mut(|m| match env.piles.surface(&m.key) {
            Some(s) => {
                m.x = s.x0 + m.offset;
                m.base = s.top_at(m.x).unwrap_or(s.y);
                true
            }
            None => {
                broken.push(m.clone());
                false
            }
        });
        for m in broken {
            self.break_snowman(&m, (0.0, 0.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{Inputs, OutputInfo, WindowRect};

    const BOUNDS: Rect = Rect::new(0, 0, 1920, 1080);

    fn piles(snow: f32) -> Piles {
        let inputs = Inputs {
            outputs: vec![OutputInfo {
                name: "A".into(),
                rect: BOUNDS,
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
        // A flat layer of snow on the panel.
        for x in (0..1920).step_by(4) {
            p.land(x as f32, 1000.0, 1050.0, 1.0, snow * 4.0, 40.0);
        }
        p
    }

    struct Rig {
        folk: Snowfolk,
        piles: Piles,
        particles: Particles,
        now: f64,
        occluders: Vec<(i32, Rect)>,
    }

    impl Rig {
        fn new(snow: f32) -> Rig {
            Rig {
                folk: Snowfolk::default(),
                piles: piles(snow),
                particles: Particles::new(1),
                now: 0.0,
                occluders: Vec::new(),
            }
        }

        fn step(&mut self, cursor: Option<CursorState>, carry: CarryModifier) {
            self.now += 1.0 / 60.0;
            let mut env = Env {
                dt: 1.0 / 60.0,
                now: self.now,
                cursor,
                carry,
                piles: &mut self.piles,
                particles: &mut self.particles,
                bounds: BOUNDS,
                max_pile_height: 40.0,
                occluders: &self.occluders,
            };
            self.folk.step(&mut env);
        }

        fn run(&mut self, secs: f32) {
            for _ in 0..(secs * 60.0) as u32 {
                self.step(None, CarryModifier::Alt);
            }
        }

        /// Sweep the cursor sideways through the panel pile.
        fn roll(&mut self, from: f32, to: f32, speed: f32) {
            let y = 1036.0;
            let dir = (to - from).signum();
            let mut x = from;
            while (to - x) * dir > 0.0 {
                x += dir * speed / 60.0;
                let c = CursorState {
                    x,
                    y,
                    ..CursorState::default()
                };
                self.step(Some(c), CarryModifier::Alt);
            }
        }

        /// Drop a new resting ball of `volume` at `x` (test helper).
        fn ball_at(&mut self, x: f32, volume: f32) -> u32 {
            let key = SurfaceKey::Obstacle(1);
            let s = self.piles.surface(&key).unwrap().clone();
            let id = self.folk.new_id();
            let r = radius_of(volume);
            self.folk.balls.push(Snowball {
                id,
                x,
                y: s.top_at(x).unwrap() - r,
                vx: 0.0,
                vy: 0.0,
                volume,
                state: State::OnSurface {
                    key,
                    offset: x - s.x0,
                },
                settled: 1.0,
            });
            id
        }
    }

    #[test]
    fn sweeping_through_snow_rolls_a_growing_ball() {
        let mut rig = Rig::new(8.0);
        let before = rig.piles.volume();
        rig.roll(200.0, 900.0, 400.0);
        assert_eq!(rig.folk.balls().len(), 1, "one ball, pushed along");
        let b = &rig.folk.balls()[0];
        assert!(b.x > 800.0, "pushed to the right: {}", b.x);
        assert!(b.radius() > 8.0, "grew while rolling: r = {}", b.radius());
        let taken = before - rig.piles.volume();
        assert!((taken - b.volume).abs() < 1.0, "snow is conserved");
        // Without the cursor it slows down and stops.
        rig.run(4.0);
        assert_eq!(rig.folk.balls()[0].vx, 0.0);
    }

    #[test]
    fn no_snow_no_ball() {
        let mut rig = Rig::new(0.0);
        rig.roll(200.0, 600.0, 400.0);
        assert!(rig.folk.balls().is_empty());
    }

    #[test]
    fn click_the_top_of_a_ball_to_carry_it() {
        let mut rig = Rig::new(8.0);
        rig.ball_at(500.0, 400.0);
        let (x, y, r) = {
            let b = &rig.folk.balls()[0];
            (b.x, b.y, b.radius())
        };
        let at = |x, y, left_down, alt| CursorState {
            x,
            y,
            left_down,
            alt,
            super_: false,
        };
        // Only the upper half is clickable without a modifier.
        let grab = rig.folk.grab_rects(None, CarryModifier::Alt);
        assert_eq!(grab.len(), 1);
        assert!(grab[0].contains(x as i32, (y - r * 0.5) as i32));
        assert!(!grab[0].contains(x as i32, (y + r * 0.5) as i32));
        // Hovering in from above doesn't push it away.
        for k in 0..20 {
            let cx = x - 40.0 + 2.0 * k as f32;
            rig.step(Some(at(cx, y - r * 0.5, false, false)), CarryModifier::Alt);
        }
        assert!((rig.folk.balls()[0].x - x).abs() < 0.5, "not pushed");
        // Plain click on its lower half (moving straight down, so no push):
        // not picked up.
        let gx = x - 2.0;
        rig.step(Some(at(gx, y + r * 0.5, true, false)), CarryModifier::Alt);
        assert!(!rig.folk.is_carrying());
        rig.step(Some(at(gx, y + r * 0.5, false, false)), CarryModifier::Alt);
        // Plain click on its upper half: carried along with the cursor.
        let gy = y - r * 0.5;
        rig.step(Some(at(gx, gy, true, false)), CarryModifier::Alt);
        assert!(rig.folk.is_carrying());
        assert!(!rig.folk.grab_rects(None, CarryModifier::Alt).is_empty());
        for k in 1..=30 {
            rig.step(
                Some(at(gx, gy - 10.0 * k as f32, true, false)),
                CarryModifier::Alt,
            );
        }
        assert!((rig.folk.balls()[0].y - (y - 300.0)).abs() < 1.0);
        // Release: it drops back onto the panel (300 px fall: poof!).
        rig.step(Some(at(gx, gy - 300.0, false, false)), CarryModifier::Alt);
        rig.run(2.0);
        assert!(rig.folk.balls().is_empty(), "poofed into the pile");
    }

    #[test]
    fn modifier_makes_the_whole_ball_clickable() {
        let mut rig = Rig::new(8.0);
        rig.ball_at(500.0, 400.0);
        let (x, y, r) = {
            let b = &rig.folk.balls()[0];
            (b.x, b.y, b.radius())
        };
        let alt = CursorState {
            x,
            y: y + r * 0.5,
            left_down: true,
            alt: true,
            super_: false,
        };
        rig.step(Some(alt), CarryModifier::Alt);
        assert!(rig.folk.is_carrying());
    }

    #[test]
    fn short_drop_lands_without_poof() {
        let mut rig = Rig::new(8.0);
        rig.ball_at(500.0, 400.0);
        rig.folk.balls[0].y -= 60.0;
        rig.folk.balls[0].state = State::Free {
            fall_from: rig.folk.balls[0].y,
        };
        rig.run(1.0);
        assert_eq!(rig.folk.balls().len(), 1);
        assert!(rig.folk.balls()[0].resting());
    }

    #[test]
    fn three_stacked_balls_make_a_snowman() {
        let mut rig = Rig::new(8.0);
        let big = rig.ball_at(800.0, 2000.0);
        let mid_vol = 900.0;
        let small_vol = 300.0;
        // Drop the medium ball onto the big one, then the small one on top.
        for vol in [mid_vol, small_vol] {
            let id = rig.folk.new_id();
            let top = rig
                .folk
                .balls()
                .iter()
                .map(|b| b.y - b.radius())
                .fold(f32::MAX, f32::min);
            let r = radius_of(vol);
            rig.folk.balls.push(Snowball {
                id,
                x: 801.0,
                y: top - r - 30.0,
                vx: 0.0,
                vy: 0.0,
                volume: vol,
                state: State::Free {
                    fall_from: top - r - 30.0,
                },
                settled: 0.0,
            });
            rig.run(1.0);
        }
        assert!(
            rig.folk.balls().iter().all(|b| b.id != big),
            "balls consumed"
        );
        assert_eq!(rig.folk.snowmen().len(), 1);
        let m = &rig.folk.snowmen()[0];
        assert!(m.radii[0] > m.radii[1] && m.radii[1] > m.radii[2]);
        assert!((m.x - 800.0).abs() < 2.0);
    }

    #[test]
    fn any_three_stacked_balls_make_a_snowman() {
        let mut rig = Rig::new(8.0);
        rig.ball_at(800.0, 300.0); // small at the bottom
        for vol in [900.0, 2000.0] {
            let top = rig
                .folk
                .balls()
                .iter()
                .map(|b| b.y - b.radius())
                .fold(f32::MAX, f32::min);
            let id = rig.folk.new_id();
            let r = radius_of(vol);
            rig.folk.balls.push(Snowball {
                id,
                x: 800.0,
                y: top - r - 30.0,
                vx: 0.0,
                vy: 0.0,
                volume: vol,
                state: State::Free {
                    fall_from: top - r - 30.0,
                },
                settled: 0.0,
            });
            rig.run(1.0);
        }
        assert_eq!(rig.folk.snowmen().len(), 1);
        let m = &rig.folk.snowmen()[0];
        assert!(
            m.radii[0] > m.radii[1] && m.radii[1] > m.radii[2],
            "biggest at the bottom: {:?}",
            m.radii
        );
        let total: f32 = m.volumes.iter().sum();
        assert!((total - 3200.0).abs() < 1.0, "snow is kept");
    }

    #[test]
    fn snowman_volumes_are_ordered_and_conserved() {
        for v in [
            [300.0, 900.0, 2000.0],
            [500.0, 500.0, 500.0],
            [2000.0, 1999.0, 10.0],
            [40.0, 40.0, 3000.0],
        ] {
            let out = snowman_volumes(v);
            let r = out.map(radius_of);
            assert!(
                r[0] > r[1] * SNOWMAN_RATIO - 1e-3 && r[1] > r[2] * SNOWMAN_RATIO - 1e-3,
                "{v:?} -> {out:?}"
            );
            assert!((out.iter().sum::<f32>() - v.iter().sum::<f32>()).abs() < 1e-2);
        }
    }

    #[test]
    fn balls_push_each_other() {
        let mut rig = Rig::new(8.0);
        rig.ball_at(500.0, 400.0);
        rig.ball_at(540.0, 400.0);
        rig.folk.balls[0].vx = 300.0;
        rig.run(1.0);
        let (a, b) = (&rig.folk.balls()[0], &rig.folk.balls()[1]);
        assert!(b.x > 545.0, "the second ball was pushed: {}", b.x);
        assert!((b.x - a.x) >= a.radius() + b.radius() - 1.0, "no overlap");
    }

    #[test]
    fn a_ball_behind_a_window_cannot_be_grabbed_or_pushed() {
        use crate::geom::AppWindow;
        let mut rig = Rig::new(8.0);
        // A snowy window (z 0) with a ball on it, and another window (z 1)
        // stacked above, covering it.
        let back = Rect::new(300, 500, 600, 300);
        let inputs = Inputs {
            outputs: vec![OutputInfo {
                name: "A".into(),
                rect: BOUNDS,
                scale: 1.0,
            }],
            windows: vec![AppWindow {
                id: 5,
                rect: back,
                collects: true,
                pid: None,
            }],
            ..Inputs::default()
        };
        rig.piles = Piles::default();
        rig.piles
            .sync(&inputs, 40.0, 1.0, 0.0, &mut Particles::new(0));
        for x in (300..900).step_by(4) {
            rig.piles.land(x as f32, 450.0, 505.0, 1.0, 32.0, 40.0);
        }
        let key = SurfaceKey::Window(5);
        let surf = rig.piles.surface(&key).unwrap().clone();
        let (x, volume) = (600.0, 400.0);
        let r = radius_of(volume);
        let y = surf.top_at(x).unwrap() - r;
        rig.folk.balls.push(Snowball {
            id: 1,
            x,
            y,
            vx: 0.0,
            vy: 0.0,
            volume,
            state: State::OnSurface {
                key,
                offset: x - surf.x0,
            },
            settled: 1.0,
        });
        rig.occluders = vec![(0, back), (1, Rect::new(0, 0, 1920, 1000))];
        let at = |x, y, left_down| CursorState {
            x,
            y,
            left_down,
            ..CursorState::default()
        };
        rig.step(Some(at(x, y - r * 0.5, false)), CarryModifier::Alt);
        rig.step(Some(at(x, y - r * 0.5, true)), CarryModifier::Alt);
        assert!(!rig.folk.is_carrying(), "hidden: not picked up");
        rig.step(Some(at(x, y - r * 0.5, false)), CarryModifier::Alt);
        // Sweeping through it at snow level doesn't push it either.
        for k in 0..30 {
            let cx = x - 30.0 + 4.0 * k as f32;
            rig.step(Some(at(cx, y + r * 0.5, false)), CarryModifier::Alt);
        }
        assert!((rig.folk.balls()[0].x - x).abs() < 0.5, "not pushed");
        assert_eq!(rig.folk.balls().len(), 1, "and nothing scooped");
        // Uncovered, the same click picks it up.
        rig.occluders = vec![(0, back)];
        rig.step(Some(at(x, y - r * 0.5, true)), CarryModifier::Alt);
        assert!(rig.folk.is_carrying());
    }
}
