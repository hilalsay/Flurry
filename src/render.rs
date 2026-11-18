//! Turns a `sim::World` into a per-output [`Scene`]: which rects to clear and
//! which flake sprites to draw where. Backends replay the scene:
//! - CPU backends (Wayland, `render-test`) call [`Renderer::rasterize`] into a
//!   `tiny_skia::Pixmap` with a small hand-written src-over blit;
//! - X11 uploads the [`SpriteSheet`] once as an XRender glyph set and draws
//!   the whole scene server-side in a few requests (no pixel copies).
//!
//! Sprites are pre-rendered soft discs: quantised radius × 4×4 subpixel offsets.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Transform};

use crate::geom::{subtract, Rect};
use crate::sim::flakes::{Flake, Layer};
use crate::sim::piles::{Surface, SurfaceKey, COL_W};
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
    images: ImageSheet,
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
            images: ImageSheet::default(),
        }
    }

    pub fn sprites(&self) -> &SpriteSheet {
        &self.sheet
    }

    pub fn images(&self) -> &ImageSheet {
        &self.images
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
        self.place_at(f.x, f.y, f.r, f.layer, output)
    }

    /// The sprite for a disc of radius `r` centred at global (`x`, `y`).
    fn place_at(&self, x: f32, y: f32, r: f32, layer: Layer, output: Rect) -> SpriteDraw {
        let (ix, qx) = quantise(x - output.x as f32);
        let (iy, qy) = quantise(y - output.y as f32);
        let sprite = sprite_index(layer, bucket(r), qx, qy);
        let half = self.sheet.get(sprite).half;
        SpriteDraw {
            sprite,
            x: ix - half,
            y: iy - half,
        }
    }
}

/// Pile colours (premultiplied-free RGBA): near-white snow, and a soft
/// blue-grey line just under it.
pub const PILE_FILL: [u8; 4] = [246, 249, 255, 255];
pub const PILE_SHADOW: [u8; 4] = [95, 115, 155, 120];
/// How far the shadow outline is shifted down (px).
pub const PILE_SHADOW_OFFSET: f32 = 1.5;
/// Redraw a column once its height changed by this much (px).
const PILE_REDRAW_STEP: f32 = 0.25;
/// Extra space around a pile that its drawing may touch (shadow, AA).
const PILE_PAD: f32 = 4.0;

/// A run of pile columns to draw, in output-local px.
#[derive(Debug, Clone, PartialEq)]
pub struct PileDraw {
    /// Left edge of the first column.
    pub x0: f32,
    /// The line the snow rests on.
    pub base_y: f32,
    /// Height per column (each `COL_W` wide).
    pub heights: Vec<f32>,
    /// Windows/panels stacked above this pile: don't draw there.
    pub holes: Vec<Rect>,
}

impl PileDraw {
    /// Top outline from left to right: column centres, plus both ends.
    pub fn outline(&self) -> Vec<(f32, f32)> {
        let n = self.heights.len();
        let mut pts = Vec::with_capacity(n + 2);
        let (first, last) = (self.heights[0], self.heights[n - 1]);
        pts.push((self.x0, self.base_y - first));
        for (i, h) in self.heights.iter().enumerate() {
            pts.push((self.x0 + (i as f32 + 0.5) * COL_W, self.base_y - h));
        }
        pts.push((self.x0 + n as f32 * COL_W, self.base_y - last));
        pts
    }

    fn visible(&self) -> bool {
        self.heights.iter().any(|&h| h > 0.2)
    }
}

/// One output's top-layer changes: clear `clear`, then draw `piles` (each
/// clipped to `clear` minus its holes), the flying snow `draws`, and the
/// `images` (snowballs, snowmen), all clipped to `clear`.
#[derive(Debug, Clone, Default)]
pub struct PileScene {
    pub clear: Vec<Rect>,
    pub piles: Vec<PileDraw>,
    pub draws: Vec<SpriteDraw>,
    pub images: Vec<ImageDraw>,
}

/// Draw image `image` of the [`ImageSheet`] with its top-left at (`x`, `y`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDraw {
    pub image: u32,
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ImageKey {
    /// Radius in half pixels.
    Ball(u16),
    /// Radii in whole pixels.
    Snowman([u16; 3]),
}

struct Image {
    pixmap: Pixmap,
    anchor: (i32, i32),
}

/// Full-colour images, created on first use and never changed, so backends
/// can upload each id once. Ids are dense: 0..len().
#[derive(Default)]
pub struct ImageSheet {
    images: RefCell<Vec<Image>>,
    index: RefCell<HashMap<ImageKey, u32>>,
}

impl ImageSheet {
    pub fn len(&self) -> usize {
        self.images.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Run `f` with image `id` (premultiplied RGBA).
    pub fn with<R>(&self, id: u32, f: impl FnOnce(&Pixmap) -> R) -> Option<R> {
        self.images.borrow().get(id as usize).map(|i| f(&i.pixmap))
    }

    /// The image for `key`, its size and anchor.
    fn get(&self, key: ImageKey) -> Option<(u32, u32, u32, (i32, i32))> {
        let known = self.index.borrow().get(&key).copied();
        let id = match known {
            Some(id) => id,
            None => {
                let (pixmap, anchor) = match key {
                    ImageKey::Ball(half) => crate::art::snowball(f32::from(half) / 2.0)?,
                    ImageKey::Snowman(r) => crate::art::snowman(r.map(f32::from))?,
                };
                let mut images = self.images.borrow_mut();
                images.push(Image { pixmap, anchor });
                let id = (images.len() - 1) as u32;
                self.index.borrow_mut().insert(key, id);
                id
            }
        };
        let images = self.images.borrow();
        let img = images.get(id as usize)?;
        Some((id, img.pixmap.width(), img.pixmap.height(), img.anchor))
    }
}

#[derive(Debug, Clone)]
struct Drawn {
    x0: f32,
    y: f32,
    heights: Vec<f32>,
    holes: Vec<Rect>,
    /// Output-local area last drawn.
    area: Rect,
}

/// Remembers what each output's piles window shows, to redraw only changes.
#[derive(Debug, Clone)]
pub struct PileTracker {
    drawn: HashMap<SurfaceKey, Drawn>,
    full: bool,
    /// Flying snow, tracked like flakes.
    particles: Damage,
    /// Images on screen last frame, with their output-local rects.
    images: Vec<(ImageDraw, Rect)>,
}

impl Default for PileTracker {
    fn default() -> Self {
        Self {
            drawn: HashMap::new(),
            full: false,
            particles: Damage::new(0, 0),
            images: Vec::new(),
        }
    }
}

impl PileTracker {
    /// Redraw everything next frame.
    pub fn invalidate(&mut self) {
        self.full = true;
        self.drawn.clear();
        self.images.clear();
    }
}

/// Occluders above surface `s` that overlap `area` (both output-local).
fn holes_for(world: &World, s: &Surface, area: Rect, output: Rect) -> Vec<Rect> {
    world
        .occluders()
        .filter(|(z, _)| *z > s.z)
        .map(|(_, r)| Rect::new(r.x - output.x, r.y - output.y, r.w, r.h))
        .filter_map(|r| r.intersect(&area))
        .collect()
}

/// The output-local rect that columns `c0..c1` of `s` may draw into.
fn pile_area(s: &Surface, c0: usize, c1: usize, max_h: f32, output: Rect) -> Rect {
    let x0 = (s.x0 + c0 as f32 * COL_W).floor() as i32 - output.x;
    let x1 = (s.x0 + c1 as f32 * COL_W).ceil() as i32 - output.x;
    let y0 = (s.y - max_h - PILE_PAD).floor() as i32 - output.y;
    let y1 = (s.y + PILE_PAD).ceil() as i32 - output.y;
    Rect::new(x0, y0, (x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32)
}

fn clip_to(r: Rect, w: u32, h: u32) -> Option<Rect> {
    r.intersect(&Rect::new(0, 0, w, h))
}

/// Columns of `s` whose height moved by `PILE_REDRAW_STEP` since `drawn`.
fn changed_span(s: &Surface, drawn: &[f32]) -> Option<(usize, usize)> {
    let mut span: Option<(usize, usize)> = None;
    for (i, (h, d)) in s.heights.iter().zip(drawn).enumerate() {
        if (h - d).abs() >= PILE_REDRAW_STEP {
            span = Some(span.map_or((i, i + 1), |(a, _)| (a, i + 1)));
        }
    }
    span
}

impl Renderer {
    /// Build the next piles frame for the output at `output` (global coords).
    pub fn build_pile_scene(
        &self,
        world: &World,
        output: Rect,
        tracker: &mut PileTracker,
        scene: &mut PileScene,
    ) {
        scene.clear.clear();
        scene.piles.clear();
        let max_h = world.params().max_pile_height;
        let (w, h) = (output.w, output.h);
        if std::mem::take(&mut tracker.full) {
            scene.clear.push(Rect::new(0, 0, w, h));
        }
        let mut seen = HashSet::new();
        for s in world.piles().surfaces() {
            let whole = pile_area(s, 0, s.heights.len(), max_h, output);
            if clip_to(whole, w, h).is_none() {
                continue;
            }
            seen.insert(s.key.clone());
            let holes = holes_for(world, s, whole, output);
            let span = match tracker.drawn.get_mut(&s.key) {
                Some(d)
                    if d.x0 == s.x0
                        && d.y == s.y
                        && d.heights.len() == s.heights.len()
                        && d.holes == holes =>
                {
                    let span = changed_span(s, &d.heights);
                    if let Some((a, b)) = span {
                        d.heights[a..b].copy_from_slice(&s.heights[a..b]);
                    }
                    span
                }
                old => {
                    // New surface, or it moved/resized: clear where it was.
                    if let Some(d) = old {
                        scene.clear.extend(clip_to(d.area, w, h));
                    }
                    tracker.drawn.insert(
                        s.key.clone(),
                        Drawn {
                            x0: s.x0,
                            y: s.y,
                            heights: s.heights.clone(),
                            holes,
                            area: whole,
                        },
                    );
                    Some((0, s.heights.len()))
                }
            };
            if let Some((a, b)) = span {
                // One column of margin each side so slopes redraw cleanly.
                let (a, b) = (a.saturating_sub(1), (b + 1).min(s.heights.len()));
                scene
                    .clear
                    .extend(clip_to(pile_area(s, a, b, max_h, output), w, h));
            }
        }
        tracker.drawn.retain(|key, d| {
            let keep = seen.contains(key);
            if !keep {
                scene.clear.extend(clip_to(d.area, w, h));
            }
            keep
        });
        self.particle_draws(world, output, &mut tracker.particles, scene);
        self.image_draws(world, output, tracker, scene);
        self.pile_draws(world, output, scene);
    }

    /// Snowballs and snowmen. Only when the set of images or their places
    /// change are their old and new rects cleared; otherwise they stay as
    /// drawn (and are redrawn only where something else cleared them).
    fn image_draws(
        &self,
        world: &World,
        output: Rect,
        tracker: &mut PileTracker,
        scene: &mut PileScene,
    ) {
        scene.images.clear();
        let mut now: Vec<(ImageDraw, Rect)> = Vec::new();
        let mut add = |key, x: f32, y: f32| {
            let Some((image, w, h, (ax, ay))) = self.images.get(key) else {
                return;
            };
            let draw = ImageDraw {
                image,
                x: x.round() as i32 - output.x - ax,
                y: y.round() as i32 - output.y - ay,
            };
            let rect = Rect::new(draw.x, draw.y, w, h);
            if clip_to(rect, output.w, output.h).is_some() {
                now.push((draw, rect));
            }
        };
        let folk = world.snowfolk();
        for m in folk.snowmen() {
            let r = m.radii.map(|r| r.round() as u16);
            add(ImageKey::Snowman(r), m.x, m.base);
        }
        for b in folk.balls() {
            let half = (b.radius() * 2.0).round() as u16;
            add(ImageKey::Ball(half), b.x, b.y);
        }
        if now != tracker.images {
            let (w, h) = (output.w, output.h);
            for (_, r) in tracker.images.iter().chain(&now) {
                scene.clear.extend(clip_to(*r, w, h));
            }
        }
        scene.images.extend(now.iter().map(|(d, _)| *d));
        tracker.images = now;
    }

    /// Flying snow as near-layer sprites; their damage joins `scene.clear`.
    fn particle_draws(
        &self,
        world: &World,
        output: Rect,
        damage: &mut Damage,
        scene: &mut PileScene,
    ) {
        if (damage.w, damage.h) != (output.w, output.h) {
            *damage = Damage::new(output.w, output.h);
        }
        // Only tiles that had or have a particle, never the whole screen.
        damage.full = false;
        damage.cur.fill(false);
        scene.draws.clear();
        let (w, h) = (output.w as i32, output.h as i32);
        for p in world.particles().iter() {
            let draw = self.place_at(p.x, p.y, p.radius(), Layer::Near, output);
            let size = self.sheet.get(draw.sprite).size as i32;
            if draw.x + size <= 0 || draw.y + size <= 0 || draw.x >= w || draw.y >= h {
                continue;
            }
            damage.mark(draw.x, draw.y, draw.x + size, draw.y + size);
            scene.draws.push(draw);
        }
        damage.compute_rects();
        scene.clear.extend_from_slice(&damage.rects);
        std::mem::swap(&mut damage.prev, &mut damage.cur);
    }

    /// Every pile part that overlaps a cleared rect (they will be clipped).
    fn pile_draws(&self, world: &World, output: Rect, scene: &mut PileScene) {
        for s in world.piles().surfaces() {
            for r in &scene.clear {
                let rx0 = (r.x + output.x) as f32;
                let rx1 = (r.right() + output.x) as f32;
                let (ry0, ry1) = ((r.y + output.y) as f32, (r.bottom() + output.y) as f32);
                if s.y + PILE_PAD < ry0 || s.y - world.params().max_pile_height - PILE_PAD > ry1 {
                    continue;
                }
                let c0 = (((rx0 - s.x0) / COL_W).floor().max(0.0) as usize).min(s.heights.len());
                let c1 = (((rx1 - s.x0) / COL_W).ceil().max(0.0) as usize).min(s.heights.len());
                if c0 >= c1 {
                    continue;
                }
                let max_h = world.params().max_pile_height;
                let whole = pile_area(s, 0, s.heights.len(), max_h, output);
                let draw = PileDraw {
                    x0: s.x0 + c0 as f32 * COL_W - output.x as f32,
                    base_y: s.y - output.y as f32,
                    heights: s.heights[c0..c1].to_vec(),
                    holes: holes_for(world, s, whole, output),
                };
                if draw.visible() {
                    scene.piles.push(draw);
                }
            }
        }
    }

    /// Replay a pile scene into `pixmap` on the CPU (render-test, Wayland).
    pub fn rasterize_piles(&self, scene: &PileScene, pixmap: &mut Pixmap) {
        let mut target = Target {
            w: pixmap.width() as i32,
            h: pixmap.height() as i32,
            data: pixmap.data_mut(),
        };
        for r in &scene.clear {
            target.clear(r);
        }
        let (pw, ph) = (pixmap.width(), pixmap.height());
        for draw in &scene.piles {
            let clip = clip_mask(&subtract(&scene.clear, &draw.holes), pw, ph);
            for (rgba, dy) in [(PILE_SHADOW, PILE_SHADOW_OFFSET), (PILE_FILL, 0.0)] {
                let Some(path) = pile_path(draw, dy) else {
                    continue;
                };
                let mut paint = Paint::default();
                paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
                pixmap.fill_path(
                    &path,
                    &paint,
                    FillRule::Winding,
                    Transform::identity(),
                    clip.as_ref(),
                );
            }
        }
        let mut target = Target {
            w: pw as i32,
            h: ph as i32,
            data: pixmap.data_mut(),
        };
        for d in &scene.draws {
            target.blit(self.sheet.get(d.sprite), d.x, d.y);
        }
        if scene.images.is_empty() {
            return;
        }
        let clip = clip_mask(&scene.clear, pw, ph);
        for d in &scene.images {
            self.images.with(d.image, |img| {
                pixmap.draw_pixmap(
                    d.x,
                    d.y,
                    img.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    clip.as_ref(),
                );
            });
        }
    }
}

/// Closed outline of a pile, shifted down by `dy`.
fn pile_path(draw: &PileDraw, dy: f32) -> Option<tiny_skia::Path> {
    let pts = draw.outline();
    let (first, last) = (pts.first()?, pts.last()?);
    let mut pb = PathBuilder::new();
    pb.move_to(first.0, draw.base_y + dy);
    for (x, y) in &pts {
        pb.line_to(*x, y + dy);
    }
    pb.line_to(last.0, draw.base_y + dy);
    pb.close();
    pb.finish()
}

fn clip_mask(rects: &[Rect], w: u32, h: u32) -> Option<tiny_skia::Mask> {
    let mut mask = tiny_skia::Mask::new(w, h)?;
    let mut pb = PathBuilder::new();
    for r in rects {
        pb.push_rect(tiny_skia::Rect::from_xywh(
            r.x as f32, r.y as f32, r.w as f32, r.h as f32,
        )?);
    }
    mask.fill_path(
        &pb.finish()?,
        FillRule::Winding,
        false,
        Transform::identity(),
    );
    Some(mask)
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

    fn world_with_panel() -> (World, crate::geom::Inputs) {
        let mut inputs = inputs_1080p();
        inputs.obstacles.push(crate::geom::WindowRect {
            id: 1,
            rect: Rect::new(0, 1040, 1920, 40),
        });
        (World::new(SimParams::default(), 5), inputs)
    }

    #[test]
    fn pile_scenes_redraw_only_changes() {
        let (mut world, inputs) = world_with_panel();
        let out = inputs.outputs[0].rect;
        let renderer = Renderer::new();
        let (mut tracker, mut scene) = (PileTracker::default(), PileScene::default());
        world.step(1.0 / 60.0, &inputs);
        renderer.build_pile_scene(&world, out, &mut tracker, &mut scene);
        assert_eq!(scene.clear.len(), 1, "first frame: the whole panel span");
        renderer.build_pile_scene(&world, out, &mut tracker, &mut scene);
        assert!(scene.clear.is_empty(), "nothing changed");

        let (mut incremental, mut full) = (
            Pixmap::new(out.w, out.h).unwrap(),
            Pixmap::new(out.w, out.h).unwrap(),
        );
        let mut max_area = 0;
        for frame in 0..3600 {
            world.step(1.0 / 60.0, &inputs);
            renderer.build_pile_scene(&world, out, &mut tracker, &mut scene);
            renderer.rasterize_piles(&scene, &mut incremental);
            if frame > 0 {
                max_area = max_area.max(scene.clear.iter().map(|r| r.w * r.h).sum::<u32>());
            }
        }
        // Compare against a full redraw (allowing the sub-threshold lag).
        let (mut t, mut s) = (PileTracker::default(), PileScene::default());
        renderer.build_pile_scene(&world, out, &mut t, &mut s);
        renderer.rasterize_piles(&s, &mut full);
        let lit = |p: &Pixmap| p.data().chunks(4).filter(|px| px[3] > 128).count() as i64;
        let (a, b) = (lit(&incremental), lit(&full));
        assert!(
            b > 1920 * 3,
            "a pile should be visible after a minute ({b} px)"
        );
        assert!((a - b).abs() < b / 50, "incremental {a} vs full {b}");
        assert!(max_area < out.w * 60, "a frame redrew {max_area} px²");
    }

    #[test]
    fn pile_outline_spans_the_columns() {
        let d = PileDraw {
            x0: 10.0,
            base_y: 100.0,
            heights: vec![1.0, 3.0],
            holes: Vec::new(),
        };
        assert_eq!(
            d.outline(),
            [(10.0, 99.0), (11.0, 99.0), (13.0, 97.0), (14.0, 97.0)]
        );
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
