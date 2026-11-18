//! `flurry render-test`: run the sim headless on a fake 1920×1080 output
//! and write each frame as a PNG over a dark background.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tiny_skia::{
    Color, GradientStop, LinearGradient, Paint, Pixmap, PixmapPaint, Point, SpreadMode, Transform,
};

use crate::config::Config;
use crate::geom::{Inputs, OutputInfo, Rect};
use crate::render::Renderer;
use crate::sim::{SimParams, World};

const W: u32 = 1920;
const H: u32 = 1080;
const DT: f32 = 1.0 / 60.0;

pub fn run(config: &Config, frames: u32, out: &Path, seed: u64) -> Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    let output = OutputInfo {
        name: "FAKE-1".into(),
        rect: Rect::new(0, 0, W, H),
        scale: 1.0,
    };
    let inputs = Inputs {
        outputs: vec![output.clone()],
        ..Inputs::default()
    };
    let mut world = World::new(SimParams::from_config(config), seed);
    let renderer = Renderer::new();
    let mut snow = Pixmap::new(W, H).context("cannot allocate pixmap")?;
    let background = background()?;
    let mut frame = background.clone();
    let (mut sim_time, mut render_time) = (Duration::ZERO, Duration::ZERO);

    for i in 0..frames {
        let t = Instant::now();
        world.step(DT, &inputs);
        sim_time += t.elapsed();

        let t = Instant::now();
        renderer.render(&world, output.rect, &mut snow);
        render_time += t.elapsed();

        frame.data_mut().copy_from_slice(background.data());
        frame.draw_pixmap(
            0,
            0,
            snow.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        let path = out.join(format!("frame_{i:04}.png"));
        frame
            .save_png(&path)
            .with_context(|| format!("cannot write {}", path.display()))?;
    }

    let n = frames.max(1);
    log::info!(
        "wrote {frames} frames to {} ({} flakes; avg sim step {:?}, avg render {:?})",
        out.display(),
        world.flakes().len(),
        sim_time / n,
        render_time / n,
    );
    Ok(())
}

/// A night-sky gradient, so white snow is visible in image viewers.
fn background() -> Result<Pixmap> {
    let mut pm = Pixmap::new(W, H).context("cannot allocate pixmap")?;
    let shader = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(0.0, H as f32),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(14, 20, 40, 255)),
            GradientStop::new(1.0, Color::from_rgba8(44, 56, 90, 255)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .context("invalid gradient")?;
    let paint = Paint {
        shader,
        ..Paint::default()
    };
    let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, W as f32, H as f32).context("bad rect")?;
    pm.fill_rect(rect, &paint, Transform::identity(), None);
    Ok(pm)
}
