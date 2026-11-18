//! The daemon: backend + world + renderer + main loop (PLAN.md §4 "Main loop").

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use crate::config::Config;
use crate::geom::{Inputs, OutputInfo};
use crate::platform::{self, Backend, BackendEvent, BackendKind, BackendOptions};
use crate::render::{Damage, Renderer, Scene};
use crate::sim::{SimParams, World};

/// Fixed simulation step.
const STEP: Duration = Duration::from_nanos(1_000_000_000 / 60);
/// Don't try to catch up more than this after a stall (e.g. suspend).
const MAX_CATCH_UP: Duration = Duration::from_millis(250);
/// Log frame timings this often at debug level.
const STATS_EVERY: Duration = Duration::from_secs(10);

pub struct RunOptions {
    pub backend: Option<BackendKind>,
    pub no_compositor_check: bool,
}

/// Per-output render state.
struct Surface {
    rect: crate::geom::Rect,
    damage: Damage,
    scene: Scene,
}

fn surfaces_for(outputs: &[OutputInfo]) -> Vec<Surface> {
    outputs
        .iter()
        .map(|o| Surface {
            rect: o.rect,
            damage: Damage::new(o.rect.w, o.rect.h),
            scene: Scene::default(),
        })
        .collect()
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

pub fn run(config: &Config, opts: &RunOptions) -> Result<()> {
    let mut backend = platform::select(
        opts.backend,
        &BackendOptions {
            no_compositor_check: opts.no_compositor_check,
            monitors: config.monitors.clone(),
        },
    )?;
    log::info!(
        "running on {} with {} monitor(s)",
        backend.name(),
        backend.outputs().len()
    );
    MainLoop::new(config, backend.as_mut())?.run()
}

struct MainLoop<'b> {
    backend: &'b mut dyn Backend,
    world: World,
    renderer: Renderer,
    surfaces: Vec<Surface>,
    inputs: Inputs,
    frame_interval: Duration,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    busy: Duration,
}

impl<'b> MainLoop<'b> {
    fn new(config: &Config, backend: &'b mut dyn Backend) -> Result<Self> {
        let inputs = Inputs {
            outputs: backend.outputs().to_vec(),
            ..Inputs::default()
        };
        Ok(Self {
            surfaces: surfaces_for(&inputs.outputs),
            inputs,
            backend,
            world: World::new(SimParams::from_config(config), seed()),
            renderer: Renderer::new(),
            frame_interval: Duration::from_secs(1) / config.fps,
            stats: Stats::default(),
        })
    }

    fn run(&mut self) -> Result<()> {
        let mut last = Instant::now();
        let mut acc = Duration::ZERO;
        let mut next_frame = last;
        loop {
            let wait = next_frame.saturating_duration_since(Instant::now());
            for event in self.backend.dispatch(wait)? {
                match event {
                    BackendEvent::OutputsChanged => self.outputs_changed()?,
                }
            }
            let now = Instant::now();
            if now < next_frame {
                continue;
            }
            acc = (acc + (now - last)).min(MAX_CATCH_UP);
            last = now;
            while acc >= STEP {
                self.world.step(STEP.as_secs_f32(), &self.inputs);
                acc -= STEP;
            }
            self.draw()?;
            self.record(now);
            next_frame += self.frame_interval;
            if next_frame < now {
                // Fell behind: don't burst frames to catch up.
                next_frame = now + self.frame_interval;
            }
        }
    }

    fn draw(&mut self) -> Result<()> {
        for (i, s) in self.surfaces.iter_mut().enumerate() {
            self.renderer
                .build_scene(&self.world, s.rect, &mut s.damage, &mut s.scene);
            self.backend.present(i, &s.scene, self.renderer.sprites())?;
        }
        Ok(())
    }

    fn outputs_changed(&mut self) -> Result<()> {
        self.inputs.outputs = self.backend.outputs().to_vec();
        self.surfaces = surfaces_for(&self.inputs.outputs);
        for o in &self.inputs.outputs {
            log::debug!("output {}: {:?}", o.name, o.rect);
        }
        Ok(())
    }

    fn record(&mut self, frame_start: Instant) {
        let s = &mut self.stats;
        s.frames += 1;
        s.busy += frame_start.elapsed();
        let since = *s.since.get_or_insert(frame_start);
        if since.elapsed() >= STATS_EVERY {
            log::debug!(
                "{} frames in {:.1?}, avg {:?} busy per frame, {} flakes",
                s.frames,
                since.elapsed(),
                s.busy / s.frames.max(1),
                self.world.flakes().len()
            );
            *s = Stats::default();
        }
    }
}
