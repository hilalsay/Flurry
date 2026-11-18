//! Draws a `sim::World` into a `tiny_skia::Pixmap`, one per output.
//!
//! Flakes are drawn from pre-rendered sprites (quantised radius × 4×4
//! subpixel offsets) with a small hand-written src-over blit. That avoids
//! setting up a tiny-skia raster pipeline per flake, which would dominate
//! the frame time with hundreds of tiny circles.

use tiny_skia::{Color, Pixmap};

use crate::geom::Rect;
use crate::sim::flakes::{Flake, Layer};
use crate::sim::World;

const SUBPIXEL: usize = 4;
const R_MIN: f32 = 0.75;
const R_STEP: f32 = 0.25;
/// Radii 0.75 ..= 4.0 px in 0.25 px steps.
const BUCKETS: usize = 14;

/// Slightly blue-grey far away, pure white up close.
fn layer_color(layer: Layer) -> [f32; 3] {
    match layer {
        Layer::Far => [205.0, 220.0, 240.0],
        Layer::Mid => [235.0, 242.0, 255.0],
        Layer::Near => [255.0, 255.0, 255.0],
    }
}

/// A premultiplied RGBA sprite of one flake at one subpixel offset.
struct Sprite {
    size: usize,
    /// Offset from the flake's integer pixel to the sprite's top-left corner.
    half: i32,
    rgba: Vec<u8>,
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Sprite {
    /// Soft disc of radius `r`, centred at (`half + fx`, `half + fy`).
    fn new(r: f32, layer: Layer, fx: f32, fy: f32) -> Sprite {
        let half = (r + 1.0).ceil() as i32;
        let size = (2 * half + 2) as usize;
        let (cx, cy) = (half as f32 + fx, half as f32 + fy);
        let color = layer_color(layer);
        let mut rgba = Vec::with_capacity(size * size * 4);
        for j in 0..size {
            for i in 0..size {
                let d = (i as f32 + 0.5 - cx).hypot(j as f32 + 0.5 - cy);
                let a = layer.alpha() * (1.0 - smoothstep(r * 0.5, r + 0.75, d));
                for c in color {
                    rgba.push((c * a).round() as u8);
                }
                rgba.push((a * 255.0).round() as u8);
            }
        }
        Sprite { size, half, rgba }
    }

    #[cfg(test)]
    fn max_alpha(&self) -> u8 {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3])
            .max()
            .unwrap_or(0)
    }
}

pub struct Renderer {
    sprites: Vec<Sprite>,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

fn bucket(r: f32) -> usize {
    (((r - R_MIN) / R_STEP).round().max(0.0) as usize).min(BUCKETS - 1)
}

fn sprite_index(layer: Layer, bucket: usize, qx: usize, qy: usize) -> usize {
    ((layer.index() * BUCKETS + bucket) * SUBPIXEL + qy) * SUBPIXEL + qx
}

/// Splits a coordinate into an integer pixel and a subpixel index.
fn quantise(v: f32) -> (i32, usize) {
    let i = v.floor();
    let q = ((v - i) * SUBPIXEL as f32).round() as usize;
    if q == SUBPIXEL {
        (i as i32 + 1, 0)
    } else {
        (i as i32, q)
    }
}

impl Renderer {
    pub fn new() -> Self {
        let mut sprites = Vec::with_capacity(Layer::ALL.len() * BUCKETS * SUBPIXEL * SUBPIXEL);
        for layer in Layer::ALL {
            for b in 0..BUCKETS {
                let r = R_MIN + b as f32 * R_STEP;
                for qy in 0..SUBPIXEL {
                    for qx in 0..SUBPIXEL {
                        let (fx, fy) = (qx as f32, qy as f32);
                        let s = SUBPIXEL as f32;
                        sprites.push(Sprite::new(r, layer, fx / s, fy / s));
                    }
                }
            }
        }
        Self { sprites }
    }

    /// Clear `pixmap` and draw the world as seen by the output at `output`
    /// (global coordinates). The pixmap should be the output's size.
    pub fn render(&self, world: &World, output: Rect, pixmap: &mut Pixmap) {
        pixmap.fill(Color::TRANSPARENT);
        let mut target = Target {
            w: pixmap.width() as i32,
            h: pixmap.height() as i32,
            data: pixmap.data_mut(),
        };
        for layer in Layer::ALL {
            for f in world.flakes().iter().filter(|f| f.layer == layer) {
                self.draw_flake(&mut target, f, output);
            }
        }
    }

    fn draw_flake(&self, target: &mut Target, f: &Flake, output: Rect) {
        let (ix, qx) = quantise(f.x - output.x as f32);
        let (iy, qy) = quantise(f.y - output.y as f32);
        let sprite = &self.sprites[sprite_index(f.layer, bucket(f.r), qx, qy)];
        target.blit(sprite, ix - sprite.half, iy - sprite.half);
    }
}

struct Target<'a> {
    w: i32,
    h: i32,
    data: &'a mut [u8],
}

/// `v * a / 255`, rounded, for 8-bit values.
fn mul_div255(v: u8, a: u8) -> u8 {
    let t = u32::from(v) * u32::from(a) + 128;
    ((t + (t >> 8)) >> 8) as u8
}

impl Target<'_> {
    /// Premultiplied src-over of `s` with its top-left at (`x0`, `y0`), clipped.
    fn blit(&mut self, s: &Sprite, x0: i32, y0: i32) {
        let size = s.size as i32;
        let (cx0, cy0) = ((-x0).max(0), (-y0).max(0));
        let (cx1, cy1) = (size.min(self.w - x0), size.min(self.h - y0));
        if cx0 >= cx1 || cy0 >= cy1 {
            return;
        }
        for sy in cy0..cy1 {
            let src_row = (sy * size) as usize * 4;
            let dst_row = ((y0 + sy) * self.w) as usize * 4;
            for sx in cx0..cx1 {
                let si = src_row + sx as usize * 4;
                let src = &s.rgba[si..si + 4];
                if src[3] == 0 {
                    continue;
                }
                let di = dst_row + (x0 + sx) as usize * 4;
                let inv = 255 - src[3];
                for (d, &c) in self.data[di..di + 4].iter_mut().zip(src) {
                    *d = c.saturating_add(mul_div255(*d, inv));
                }
            }
        }
    }
}

/// Copy premultiplied RGBA (tiny-skia) into premultiplied BGRA, the in-memory
/// layout of X11 32-bit ARGB visuals and Wayland `ARGB8888` on little-endian.
pub fn copy_rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    debug_assert_eq!(src.len(), dst.len());
    for (s, d) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
    {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::test_util::inputs_1080p;
    use crate::sim::SimParams;

    #[test]
    fn rgba_to_bgra_swaps_red_and_blue() {
        let src = [10, 20, 30, 40, 1, 2, 3, 4];
        let mut dst = [0u8; 8];
        copy_rgba_to_bgra(&src, &mut dst);
        assert_eq!(dst, [30, 20, 10, 40, 3, 2, 1, 4]);
        // As a little-endian u32 that is 0xAARRGGBB, i.e. ARGB8888.
        assert_eq!(
            u32::from_le_bytes([dst[0], dst[1], dst[2], dst[3]]),
            0x280A141E
        );
    }

    #[test]
    fn mul_div255_is_exact_at_extremes() {
        assert_eq!(mul_div255(255, 255), 255);
        assert_eq!(mul_div255(255, 0), 0);
        assert_eq!(mul_div255(200, 128), 100);
    }

    #[test]
    fn quantise_rounds_to_subpixels() {
        assert_eq!(quantise(3.0), (3, 0));
        assert_eq!(quantise(3.26), (3, 1));
        assert_eq!(quantise(3.9), (4, 0));
        assert_eq!(quantise(-0.25), (-1, 3));
    }

    #[test]
    fn sprites_are_premultiplied_and_ordered_by_depth() {
        let r = Renderer::new();
        for s in &r.sprites {
            for p in s.rgba.as_chunks::<4>().0.iter() {
                assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
            }
        }
        let far = &r.sprites[sprite_index(Layer::Far, bucket(1.0), 0, 0)];
        let near = &r.sprites[sprite_index(Layer::Near, bucket(3.5), 0, 0)];
        assert!(near.size > far.size);
        assert!(near.max_alpha() > far.max_alpha());
    }

    #[test]
    fn renders_snow_and_clips_at_edges() {
        let mut world = World::new(SimParams::default(), 1);
        let inputs = inputs_1080p();
        world.step(1.0 / 60.0, &inputs);
        let renderer = Renderer::new();
        let mut pm = Pixmap::new(1920, 1080).unwrap();
        renderer.render(&world, inputs.outputs[0].rect, &mut pm);
        let lit = pm
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 0)
            .count();
        assert!(lit > 1000, "only {lit} pixels drawn");

        // An output far away from every flake stays empty...
        renderer.render(&world, Rect::new(10_000, 0, 1920, 1080), &mut pm);
        assert!(pm.data().iter().all(|&b| b == 0));
        // ...and one offset so flakes straddle every edge doesn't panic.
        let mut small = Pixmap::new(37, 23).unwrap();
        for x in (0..1920).step_by(30) {
            renderer.render(&world, Rect::new(x, x / 2, 37, 23), &mut small);
        }
    }

    /// Timing only: `cargo test --release -- --ignored --nocapture bench_render`
    #[test]
    #[ignore]
    fn bench_render_1080p() {
        let mut world = World::new(SimParams::default(), 1);
        let inputs = inputs_1080p();
        world.step(1.0 / 60.0, &inputs);
        let renderer = Renderer::new();
        let mut pm = Pixmap::new(1920, 1080).unwrap();
        let t = std::time::Instant::now();
        for _ in 0..200 {
            world.step(1.0 / 60.0, &inputs);
            renderer.render(&world, inputs.outputs[0].rect, &mut pm);
        }
        eprintln!(
            "full 1080p frame (clear + 300 flakes): {:?}",
            t.elapsed() / 200
        );
    }
}
