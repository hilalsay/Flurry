//! Turns a `sim::World` into a per-output [`Scene`]: which rects to clear and
//! which flake sprites to draw where. Backends replay the scene:
//! - CPU backends (Wayland, `render-test`) call [`Renderer::rasterize`] into a
//!   `tiny_skia::Pixmap` with a small hand-written src-over blit;
//! - X11 uploads the [`SpriteSheet`] once as an XRender glyph set and draws
//!   the whole scene server-side in a few requests (no pixel copies).
//!
//! Sprites are pre-rendered soft discs: quantised radius × 4×4 subpixel offsets.

use tiny_skia::{Color, Pixmap};

use crate::geom::Rect;
use crate::sim::flakes::{Flake, Layer};
use crate::sim::World;

const SUBPIXEL: usize = 4;
const R_MIN: f32 = 0.75;
const R_STEP: f32 = 0.25;
/// Radii 0.75 ..= 4.0 px in 0.25 px steps.
const BUCKETS: usize = 14;

/// Flake colour per layer: slightly blue-grey far away, pure white up close.
pub fn layer_rgb(layer: Layer) -> [u8; 3] {
    match layer {
        Layer::Far => [205, 220, 240],
        Layer::Mid => [235, 242, 255],
        Layer::Near => [255, 255, 255],
    }
}

/// One flake image at one radius and subpixel offset.
#[derive(Debug, Clone)]
pub struct Sprite {
    /// Width and height in pixels (sprites are square).
    pub size: u32,
    pub layer: Layer,
    /// Offset from the flake's integer pixel to the sprite's top-left corner.
    half: i32,
    /// Coverage, `size × size` bytes, row-major. Includes the layer's alpha.
    alpha: Vec<u8>,
    /// Premultiplied RGBA: `layer_rgb × alpha`.
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
        let size = (2 * half + 2) as u32;
        let (cx, cy) = (half as f32 + fx, half as f32 + fy);
        let rgb = layer_rgb(layer);
        let n = (size * size) as usize;
        let (mut alpha, mut rgba) = (Vec::with_capacity(n), Vec::with_capacity(n * 4));
        for j in 0..size {
            for i in 0..size {
                let d = (i as f32 + 0.5 - cx).hypot(j as f32 + 0.5 - cy);
                let cov = layer.alpha() * (1.0 - smoothstep(r * 0.5, r + 0.75, d));
                let a = (cov * 255.0).round() as u8;
                alpha.push(a);
                rgba.extend(rgb.map(|c| mul_div255(c, a)));
                rgba.push(a);
            }
        }
        Sprite {
            size,
            layer,
            half,
            alpha,
            rgba,
        }
    }

    pub fn alpha(&self) -> &[u8] {
        &self.alpha
    }

    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// Every flake sprite, indexed by `SpriteDraw::sprite`.
#[derive(Debug, Clone)]
pub struct SpriteSheet {
    sprites: Vec<Sprite>,
}

impl SpriteSheet {
    fn new() -> Self {
        let mut sprites = Vec::with_capacity(Layer::ALL.len() * BUCKETS * SUBPIXEL * SUBPIXEL);
        let s = SUBPIXEL as f32;
        for layer in Layer::ALL {
            for b in 0..BUCKETS {
                let r = R_MIN + b as f32 * R_STEP;
                for qy in 0..SUBPIXEL {
                    for qx in 0..SUBPIXEL {
                        sprites.push(Sprite::new(r, layer, qx as f32 / s, qy as f32 / s));
                    }
                }
            }
        }
        Self { sprites }
    }

    pub fn len(&self) -> usize {
        self.sprites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sprites.is_empty()
    }

    pub fn get(&self, id: u32) -> &Sprite {
        &self.sprites[id as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &Sprite)> {
        self.sprites.iter().enumerate().map(|(i, s)| (i as u32, s))
    }
}

fn bucket(r: f32) -> usize {
    (((r - R_MIN) / R_STEP).round().max(0.0) as usize).min(BUCKETS - 1)
}

fn sprite_index(layer: Layer, bucket: usize, qx: usize, qy: usize) -> u32 {
    (((layer.index() * BUCKETS + bucket) * SUBPIXEL + qy) * SUBPIXEL + qx) as u32
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

/// Draw sprite `sprite` with its top-left corner at (`x`, `y`), output-local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpriteDraw {
    pub sprite: u32,
    pub x: i32,
    pub y: i32,
}

/// One output's frame: clear `clear` to transparent, then draw `draws` in
/// order (back to front). Every draw lies inside the cleared area, so
/// replaying scenes one after another reproduces a full redraw exactly.
#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub clear: Vec<Rect>,
    pub draws: Vec<SpriteDraw>,
}

pub struct Renderer {
    sheet: SpriteSheet,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            sheet: SpriteSheet::new(),
        }
    }

    pub fn sprites(&self) -> &SpriteSheet {
        &self.sheet
    }

    /// Build the next frame for the output at `output` (global coordinates).
    /// `damage` remembers the previous frame; use one per output.
    pub fn build_scene(&self, world: &World, output: Rect, damage: &mut Damage, scene: &mut Scene) {
        if (damage.w, damage.h) != (output.w, output.h) {
            *damage = Damage::new(output.w, output.h);
        }
        damage.cur.fill(false);
        scene.draws.clear();
        let (w, h) = (output.w as i32, output.h as i32);
        for layer in Layer::ALL {
            for f in world.flakes().iter().filter(|f| f.layer == layer) {
                let draw = self.place(f, output);
                let size = self.sheet.get(draw.sprite).size as i32;
                if draw.x + size <= 0 || draw.y + size <= 0 || draw.x >= w || draw.y >= h {
                    continue;
                }
                damage.mark(draw.x, draw.y, draw.x + size, draw.y + size);
                scene.draws.push(draw);
            }
        }
        damage.compute_rects();
        scene.clear.clone_from(&damage.rects);
        std::mem::swap(&mut damage.prev, &mut damage.cur);
        damage.full = false;
    }

    /// Replay `scene` into `pixmap` (which holds the previous frame).
    pub fn rasterize(&self, scene: &Scene, pixmap: &mut Pixmap) {
        let mut target = Target {
            w: pixmap.width() as i32,
            h: pixmap.height() as i32,
            data: pixmap.data_mut(),
        };
        for r in &scene.clear {
            target.clear(r);
        }
        for d in &scene.draws {
            target.blit(self.sheet.get(d.sprite), d.x, d.y);
        }
    }

    /// Full redraw of `world` into `pixmap`, sized like `output`.
    pub fn render(&self, world: &World, output: Rect, pixmap: &mut Pixmap) {
        pixmap.fill(Color::TRANSPARENT);
        let mut scene = Scene::default();
        self.build_scene(world, output, &mut Damage::new(0, 0), &mut scene);
        self.rasterize(&scene, pixmap);
    }

    fn place(&self, f: &Flake, output: Rect) -> SpriteDraw {
        let (ix, qx) = quantise(f.x - output.x as f32);
        let (iy, qy) = quantise(f.y - output.y as f32);
        let sprite = sprite_index(f.layer, bucket(f.r), qx, qy);
        let half = self.sheet.get(sprite).half;
        SpriteDraw {
            sprite,
            x: ix - half,
            y: iy - half,
        }
    }
}

/// Side of a damage tile in pixels. The largest flake sprite is 12 px.
pub const TILE: u32 = 16;

/// Tile-based damage tracking for one output.
#[derive(Debug, Clone)]
pub struct Damage {
    w: u32,
    h: u32,
    cols: usize,
    rows: usize,
    /// Tiles holding a flake in the previous / current frame.
    prev: Vec<bool>,
    cur: Vec<bool>,
    /// Redraw everything next frame (new output, resize).
    full: bool,
    rects: Vec<Rect>,
    row_runs: [Vec<usize>; 2],
}

impl Damage {
    pub fn new(w: u32, h: u32) -> Self {
        let cols = w.div_ceil(TILE) as usize;
        let rows = h.div_ceil(TILE) as usize;
        Self {
            w,
            h,
            cols,
            rows,
            prev: vec![false; cols * rows],
            cur: vec![false; cols * rows],
            full: true,
            rects: Vec::new(),
            row_runs: [Vec::new(), Vec::new()],
        }
    }

    /// Force a full redraw on the next frame.
    pub fn invalidate(&mut self) {
        self.full = true;
    }

    /// Mark the tiles under pixel rect [x0, x1) × [y0, y1), clipped.
    fn mark(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (w, h) = (self.w as i32, self.h as i32);
        let (x0, y0, x1, y1) = (x0.max(0), y0.max(0), x1.min(w), y1.min(h));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let t = TILE as i32;
        for ty in (y0 / t) as usize..=((y1 - 1) / t) as usize {
            let row = &mut self.cur[ty * self.cols..(ty + 1) * self.cols];
            row[(x0 / t) as usize..=((x1 - 1) / t) as usize].fill(true);
        }
    }

    fn is_dirty(&self, i: usize) -> bool {
        self.full || self.prev[i] || self.cur[i]
    }

    /// Turn dirty tiles into rects: horizontal runs per tile row, merged
    /// downwards when the run below has the same span.
    fn compute_rects(&mut self) {
        self.rects.clear();
        let [mut above, mut this] = std::mem::take(&mut self.row_runs);
        above.clear();
        for ty in 0..self.rows {
            this.clear();
            let y = ty as u32 * TILE;
            let h = TILE.min(self.h - y);
            let mut a = 0;
            let mut c = 0;
            while c < self.cols {
                if !self.is_dirty(ty * self.cols + c) {
                    c += 1;
                    continue;
                }
                let start = c;
                while c < self.cols && self.is_dirty(ty * self.cols + c) {
                    c += 1;
                }
                let x = start as u32 * TILE;
                let w = (c as u32 * TILE).min(self.w) - x;
                while a < above.len() && (self.rects[above[a]].x as u32) < x {
                    a += 1;
                }
                match above.get(a).copied() {
                    Some(i) if self.rects[i].x as u32 == x && self.rects[i].w == w => {
                        self.rects[i].h += h;
                        this.push(i);
                        a += 1;
                    }
                    _ => {
                        self.rects.push(Rect::new(x as i32, y as i32, w, h));
                        this.push(self.rects.len() - 1);
                    }
                }
            }
            std::mem::swap(&mut above, &mut this);
        }
        self.row_runs = [above, this];
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
    fn clear(&mut self, r: &Rect) {
        let x0 = r.x.clamp(0, self.w) as usize * 4;
        let x1 = r.right().clamp(0, self.w) as usize * 4;
        let stride = self.w as usize * 4;
        for row in r.y.clamp(0, self.h)..r.bottom().clamp(0, self.h) {
            let off = row as usize * stride;
            self.data[off + x0..off + x1].fill(0);
        }
    }

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
    // Whole-pixel u32 ops; the compiler vectorises this loop.
    for (s, d) in src
        .as_chunks::<4>()
        .0
        .iter()
        .zip(dst.as_chunks_mut::<4>().0.iter_mut())
    {
        let v = u32::from_le_bytes(*s);
        let swapped = (v & 0xFF00_FF00) | ((v & 0xFF) << 16) | ((v >> 16) & 0xFF);
        *d = swapped.to_le_bytes();
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
        let sheet = SpriteSheet::new();
        for (_, s) in sheet.iter() {
            assert_eq!(s.alpha.len(), (s.size * s.size) as usize);
            for (p, &a) in s.rgba.as_chunks::<4>().0.iter().zip(&s.alpha) {
                assert_eq!(p[3], a);
                assert!(p[0] <= a && p[1] <= a && p[2] <= a);
            }
        }
        let far = sheet.get(sprite_index(Layer::Far, bucket(1.0), 0, 0));
        let near = sheet.get(sprite_index(Layer::Near, bucket(3.5), 0, 0));
        let max = |s: &Sprite| s.alpha.iter().copied().max().unwrap_or(0);
        assert!(near.size > far.size);
        assert!(max(near) > max(far));
        assert_eq!((far.layer, near.layer), (Layer::Far, Layer::Near));
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

    #[test]
    fn damaged_scenes_match_full_render() {
        let mut world = World::new(SimParams::default(), 11);
        let inputs = inputs_1080p();
        let out = inputs.outputs[0].rect;
        let renderer = Renderer::new();
        let mut incremental = Pixmap::new(out.w, out.h).unwrap();
        let mut full = Pixmap::new(out.w, out.h).unwrap();
        let mut damage = Damage::new(out.w, out.h);
        let mut scene = Scene::default();
        for frame in 0..240 {
            world.step(1.0 / 60.0, &inputs);
            renderer.build_scene(&world, out, &mut damage, &mut scene);
            renderer.rasterize(&scene, &mut incremental);
            if frame == 0 {
                assert_eq!(scene.clear, [out], "first frame must be a full redraw");
            } else {
                let area: u32 = scene.clear.iter().map(|r| r.w * r.h).sum();
                assert!(area < out.w * out.h / 3, "frame {frame}: damage too big");
                for (i, a) in scene.clear.iter().enumerate() {
                    for b in &scene.clear[i + 1..] {
                        assert_eq!(a.intersect(b), None, "overlapping rects");
                    }
                }
            }
            if frame % 20 == 0 {
                renderer.render(&world, out, &mut full);
                assert!(incremental.data() == full.data(), "frame {frame} differs");
            }
        }
    }

    #[test]
    fn damage_merges_runs_vertically() {
        let mut d = Damage::new(100, 50);
        d.full = false;
        d.mark(20, 0, 40, 50); // tiles x 1..=2, every row
        d.compute_rects();
        assert_eq!(d.rects, [Rect::new(16, 0, 32, 50)]);
        d.invalidate();
        d.compute_rects();
        assert_eq!(d.rects, [Rect::new(0, 0, 100, 50)]);
    }

    /// Timing only: `cargo test --release -- --ignored --nocapture bench_render`
    #[test]
    #[ignore]
    fn bench_render_1080p() {
        let mut world = World::new(SimParams::default(), 1);
        let inputs = inputs_1080p();
        let out = inputs.outputs[0].rect;
        world.step(1.0 / 60.0, &inputs);
        let renderer = Renderer::new();
        let mut pm = Pixmap::new(1920, 1080).unwrap();
        let (mut damage, mut scene) = (Damage::new(out.w, out.h), Scene::default());
        let (mut build, mut raster, mut rects) =
            (std::time::Duration::ZERO, std::time::Duration::ZERO, 0);
        for _ in 0..200 {
            world.step(1.0 / 60.0, &inputs);
            let t = std::time::Instant::now();
            renderer.build_scene(&world, out, &mut damage, &mut scene);
            build += t.elapsed();
            let t = std::time::Instant::now();
            renderer.rasterize(&scene, &mut pm);
            raster += t.elapsed();
            rects += scene.clear.len();
        }
        let n: u32 = 200;
        eprintln!(
            "1080p, 300 flakes: build_scene {:?}, rasterize {:?}, {} clear rects",
            build / n,
            raster / n,
            rects / 200
        );
    }
}
