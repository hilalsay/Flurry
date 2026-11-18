//! Settled snow: one heightmap per surface (screen bottom, panel tops, and
//! window tops from Phase 5). Columns are `COL_W` px wide; heights in px.
//! Snow lands from flakes, slides downhill when too steep, and melts.

use std::collections::HashMap;

use crate::geom::{Inputs, WindowRect};

/// Column width in px.
pub const COL_W: f32 = 2.0;
/// Neighbouring columns may differ by this much (px) before snow slides.
const MAX_STEP: f32 = 1.0;
/// Snow creep per step: smooths small bumps (diffusion; must stay < 0.5).
const CREEP: f32 = 0.06;
/// A landed flake spreads its snow over ± this many radii.
const SPREAD: f32 = 2.5;
/// Fraction of the excess moved per step (relaxation keeps it smooth).
const SLUMP_RATE: f32 = 0.5;
/// Snow volume added per landed flake: π r² × this (px²).
const FLAKE_GAIN: f32 = 1.2;
/// Melting at `melt_rate = 1`: a constant part plus a part proportional
/// to height, so tall piles shrink faster (px/s).
const MELT_BASE: f32 = 0.012;
const MELT_PER_PX: f32 = 0.0035;

/// Identifies a surface across frames, so its snow survives layout changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SurfaceKey {
    /// A stretch of a monitor's bottom edge not covered by a panel.
    Ground { output: String, segment: u32 },
    /// The top edge of a panel/dock (by window id).
    Obstacle(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub key: SurfaceKey,
    /// Left edge, global px.
    pub x0: f32,
    /// The line snow rests on (panel top, screen bottom), global px.
    pub y: f32,
    /// Snow height per column, px.
    pub heights: Vec<f32>,
}

impl Surface {
    fn new(spec: &Spec) -> Surface {
        Surface {
            key: spec.key.clone(),
            x0: spec.x0,
            y: spec.y,
            heights: vec![0.0; columns(spec.x0, spec.x1)],
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

    fn col(&self, x: f32) -> Option<usize> {
        let c = ((x - self.x0) / COL_W).floor();
        (c >= 0.0 && (c as usize) < self.heights.len()).then_some(c as usize)
    }

    /// Move onto a new span, keeping the snow at the same global x.
    fn refit(&mut self, spec: &Spec) {
        let old = std::mem::take(&mut self.heights);
        let old_x0 = self.x0;
        self.x0 = spec.x0;
        self.y = spec.y;
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
    }

    /// Let snow slide between neighbours steeper than `MAX_STEP`, and creep
    /// a little everywhere. The ends are walls (screen edges, panel ends),
    /// so the volume is unchanged.
    fn slump(&mut self) {
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
}

/// All piles.
#[derive(Debug, Clone, Default)]
pub struct Piles {
    surfaces: Vec<Surface>,
}

impl Piles {
    pub fn surfaces(&self) -> &[Surface] {
        &self.surfaces
    }

    /// Total snow on all surfaces (px²).
    pub fn volume(&self) -> f32 {
        self.surfaces.iter().map(Surface::volume).sum()
    }

    pub fn clear(&mut self) {
        for s in &mut self.surfaces {
            s.heights.fill(0.0);
        }
    }

    /// Match the surfaces to the current monitors and panels, keeping the
    /// snow of surfaces that still exist. Vanished surfaces lose their snow.
    pub fn sync(&mut self, inputs: &Inputs, max_height: f32) {
        let mut old: HashMap<SurfaceKey, Surface> = self
            .surfaces
            .drain(..)
            .map(|s| (s.key.clone(), s))
            .collect();
        self.surfaces = specs(inputs, max_height)
            .iter()
            .map(|spec| match old.remove(&spec.key) {
                Some(mut s) => {
                    if (s.x0, s.x1(), s.y) != (spec.x0, spec.x1, spec.y) {
                        s.refit(spec);
                    }
                    s
                }
                None => Surface::new(spec),
            })
            .collect();
    }

    /// If a flake of radius `r` at `x` moving from `y_prev` to `y` (centre)
    /// reaches a pile, add its snow there and return true.
    pub fn try_land(&mut self, x: f32, y_prev: f32, y: f32, r: f32, max_height: f32) -> bool {
        let (prev_bottom, bottom) = (y_prev + r, y + r);
        // The first surface the flake meets on its way down.
        let mut hit: Option<(usize, f32)> = None;
        for (i, s) in self.surfaces.iter().enumerate() {
            let Some(c) = s.col(x) else { continue };
            let top = s.y - s.heights[c];
            if prev_bottom <= top + 0.5 && bottom >= top && hit.is_none_or(|(_, t)| top < t) {
                hit = Some((i, top));
            }
        }
        let Some((i, _)) = hit else { return false };
        let s = &mut self.surfaces[i];
        let reach = r * SPREAD;
        let (Some(c0), Some(c1)) = (
            s.col((x - reach).max(s.x0)),
            s.col((x + reach).min(s.x1() - 0.01)),
        ) else {
            return false;
        };
        // Triangle-shaped deposit centred on the flake.
        let (x0, ramp) = (s.x0, reach + COL_W);
        let weight = |c: usize| {
            let cx = x0 + (c as f32 + 0.5) * COL_W;
            (1.0 - (cx - x).abs() / ramp).max(0.0)
        };
        let total: f32 = (c0..=c1).map(weight).sum::<f32>().max(1e-3);
        let volume = std::f32::consts::PI * r * r * FLAKE_GAIN / COL_W;
        for (c, h) in s.heights.iter_mut().enumerate().take(c1 + 1).skip(c0) {
            *h = (*h + volume * weight(c) / total).min(max_height);
        }
        true
    }

    /// Slide and melt.
    pub fn step(&mut self, dt: f32, melt_rate: f32, max_height: f32) {
        for s in &mut self.surfaces {
            s.slump();
            s.melt(dt, melt_rate, max_height);
        }
    }
}

/// Surfaces for the current layout:
/// - each panel's top edge, if it has room above it on its monitor;
/// - each monitor's bottom edge, minus the stretches covered by panels
///   that sit on that edge.
fn specs(inputs: &Inputs, max_height: f32) -> Vec<Spec> {
    let mut specs = Vec::new();
    for o in &inputs.obstacles {
        let r = o.rect;
        let cx = r.x + r.w as i32 / 2;
        let room_above = inputs
            .outputs
            .iter()
            .any(|out| out.rect.contains(cx, r.y) && r.y as f32 > out.rect.y as f32 + max_height);
        if room_above && r.w > 0 {
            specs.push(Spec {
                key: SurfaceKey::Obstacle(o.id),
                x0: r.x as f32,
                x1: r.right() as f32,
                y: r.y as f32,
            });
        }
    }
    for out in &inputs.outputs {
        let bottom = out.rect.bottom();
        let mut covered: Vec<(i32, i32)> = inputs
            .obstacles
            .iter()
            .map(|o: &WindowRect| o.rect)
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
    use crate::geom::{OutputInfo, Rect};

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
        p.sync(&layout(Some(BOTTOM_PANEL)), 40.0);
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
        for _ in 0..5000 {
            p.step(1.0 / 60.0, 0.0, 40.0);
        }
        assert!(
            (p.volume() - before).abs() < 1e-2,
            "{} vs {before}",
            p.volume()
        );
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
        for _ in 0..600 {
            p.step(1.0 / 60.0, 1.0, 40.0);
        }
        assert!(p.volume() < before);
        p.step(1e6, 1.0, 40.0);
        assert_eq!(p.volume(), 0.0);
    }

    #[test]
    fn height_never_exceeds_max() {
        let mut p = piles_with_panel();
        for i in 0..20_000 {
            let x = 900.0 + (i % 40) as f32;
            p.try_land(x, 990.0, 1045.0, 4.0, 25.0);
            p.step(1.0 / 60.0, 0.0, 25.0);
        }
        assert!(p.surfaces[0].max_height() <= 25.0);
        assert!(p.surfaces[0].max_height() > 20.0);
    }

    #[test]
    fn layout_changes_keep_snow() {
        let mut p = piles_with_panel();
        p.surfaces[0].heights[100] = 12.0;
        // Panel grows taller: same surface, moved up.
        p.sync(&layout(Some(Rect::new(0, 1030, 1920, 50))), 40.0);
        assert_eq!(p.surfaces().len(), 1);
        assert_eq!(p.surfaces[0].y, 1030.0);
        assert_eq!(p.surfaces[0].heights[100], 12.0);
        // Panel removed: its snow is gone, the ground appears.
        p.sync(&layout(None), 40.0);
        assert!(matches!(p.surfaces[0].key, SurfaceKey::Ground { .. }));
        assert_eq!(p.volume(), 0.0);
    }
}
