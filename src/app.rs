//! The daemon: backend + world + renderer + controls + main loop
//! (PLAN.md §4 "Main loop").

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config::{Config, Intensity};
use crate::geom::{Inputs, OutputInfo, Rect};
use crate::ipc::{self, Command, Request};
use crate::platform::{self, Backend, BackendEvent, BackendKind, BackendOptions, Frame, Hotkey};
use crate::render::{Damage, PileScene, PileTracker, Renderer, Scene};
use crate::sim::{SimParams, World};
use crate::tray::{Tray, TrayState};

/// Fixed simulation step.
const STEP: Duration = Duration::from_nanos(1_000_000_000 / 60);
/// Don't try to catch up more than this after a stall (e.g. suspend).
const MAX_CATCH_UP: Duration = Duration::from_millis(250);
/// Longest sleep between checks for commands (they don't wake the X poll).
const MAX_WAIT: Duration = Duration::from_millis(100);
/// Log frame timings this often at debug level.
const STATS_EVERY: Duration = Duration::from_secs(10);

pub struct RunOptions {
    pub backend: Option<BackendKind>,
    pub no_compositor_check: bool,
    pub config_path: PathBuf,
    pub socket_path: PathBuf,
}

/// Per-output render state.
struct Surface {
    rect: Rect,
    damage: Damage,
    scene: Scene,
    piles: PileTracker,
    pile_scene: PileScene,
}

impl Surface {
    /// The next frame starts from scratch (window content lost).
    fn invalidate(&mut self) {
        self.damage.invalidate();
        self.piles.invalidate();
    }
}

fn surfaces_for(outputs: &[OutputInfo]) -> Vec<Surface> {
    outputs
        .iter()
        .map(|o| Surface {
            rect: o.rect,
            damage: Damage::new(o.rect.w, o.rect.h),
            scene: Scene::default(),
            piles: PileTracker::default(),
            pile_scene: PileScene::default(),
        })
        .collect()
}

fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

fn frame_interval(fps: u32) -> Duration {
    Duration::from_secs(1) / fps.clamp(1, 240)
}

/// Ctrl+C / SIGTERM quit cleanly (removing the socket); SIGHUP reloads.
fn spawn_signal_thread(tx: Sender<Request>) -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM])
        .context("cannot install signal handlers")?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                let cmd = if sig == SIGHUP {
                    Command::Reload
                } else {
                    Command::Quit
                };
                if tx.send(Request::fire(cmd)).is_err() {
                    break;
                }
            }
        })?;
    Ok(())
}

pub fn run(config: Config, opts: &RunOptions) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    // Bind the socket first: it is also the single-instance check.
    let _server = ipc::Server::start(opts.socket_path.clone(), tx.clone())?;
    spawn_signal_thread(tx.clone())?;
    let hotkey = Hotkey::parse(&config.hotkey).unwrap_or_else(|e| {
        log::warn!("ignoring hotkey: {e:#}");
        None
    });
    let mut backend = platform::select(
        opts.backend,
        &BackendOptions {
            no_compositor_check: opts.no_compositor_check,
            monitors: config.monitors.clone(),
            hotkey,
            layering: config.layering,
        },
    )?;
    log::info!(
        "running on {} with {} monitor(s)",
        backend.name(),
        backend.outputs().len()
    );
    let tray = Tray::spawn(tx, tray_state(&config, false));
    let mut main = MainLoop::new(config, opts.config_path.clone(), backend.as_mut(), rx, tray);
    let result = main.run();
    log::info!("stopping");
    result
}

fn tray_state(config: &Config, paused: bool) -> TrayState {
    TrayState {
        paused,
        intensity: config.intensity,
        fps: config.fps,
    }
}

struct MainLoop<'b> {
    backend: &'b mut dyn Backend,
    config: Config,
    config_path: PathBuf,
    world: World,
    renderer: Renderer,
    surfaces: Vec<Surface>,
    inputs: Inputs,
    frame_interval: Duration,
    paused: bool,
    commands: Receiver<Request>,
    tray: Option<Tray>,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    busy: Duration,
}

/// What the main loop should do after a command.
enum Flow {
    Continue,
    Quit,
}

impl<'b> MainLoop<'b> {
    fn new(
        config: Config,
        config_path: PathBuf,
        backend: &'b mut dyn Backend,
        commands: Receiver<Request>,
        tray: Option<Tray>,
    ) -> Self {
        let inputs = Inputs {
            outputs: backend.outputs().to_vec(),
            obstacles: backend.obstacles().to_vec(),
            windows: backend.windows().to_vec(),
            hidden_windows: backend.hidden_windows().to_vec(),
            ..Inputs::default()
        };
        Self {
            surfaces: surfaces_for(&inputs.outputs),
            inputs,
            backend,
            world: World::new(SimParams::from_config(&config), seed()),
            renderer: Renderer::new(),
            frame_interval: frame_interval(config.fps),
            config,
            config_path,
            paused: false,
            commands,
            tray,
            stats: Stats::default(),
        }
    }

    fn run(&mut self) -> Result<()> {
        let mut last = Instant::now();
        let mut acc = Duration::ZERO;
        let mut next_frame = last;
        loop {
            let until_frame = next_frame.saturating_duration_since(Instant::now());
            for event in self.backend.dispatch(until_frame.min(MAX_WAIT))? {
                match event {
                    BackendEvent::OutputsChanged => self.outputs_changed(),
                    BackendEvent::Hotkey => self.set_paused(!self.paused)?,
                    BackendEvent::ObstaclesChanged => {
                        self.inputs.obstacles = self.backend.obstacles().to_vec();
                    }
                    BackendEvent::WindowsChanged => self.windows_changed(),
                    BackendEvent::Redraw(i) => {
                        if let Some(s) = self.surfaces.get_mut(i) {
                            s.invalidate();
                        }
                    }
                }
            }
            if let Flow::Quit = self.handle_commands()? {
                return Ok(());
            }
            let now = Instant::now();
            if self.paused {
                // Resume without a jump in the simulation.
                (last, next_frame) = (now, now);
                continue;
            }
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

    fn handle_commands(&mut self) -> Result<Flow> {
        while let Ok(req) = self.commands.try_recv() {
            log::debug!("command: {}", req.cmd.encode());
            let (answer, flow) = match self.execute(req.cmd) {
                Ok(r) => r,
                Err(e) => (format!("error: {e:#}"), Flow::Continue),
            };
            if let Some(reply) = req.reply {
                let _ = reply.send(answer);
            }
            if let Flow::Quit = flow {
                return Ok(Flow::Quit);
            }
        }
        Ok(Flow::Continue)
    }

    fn execute(&mut self, cmd: Command) -> Result<(String, Flow)> {
        let answer = match cmd {
            Command::Toggle => {
                self.set_paused(!self.paused)?;
                if self.paused { "paused" } else { "resumed" }.to_owned()
            }
            Command::Clear => {
                self.world.clear_settled();
                "cleared".into()
            }
            Command::Intensity(level) => {
                self.set_intensity(level);
                format!("intensity {}", ipc::intensity_name(level))
            }
            Command::Fps(fps) => {
                anyhow::ensure!((1..=240).contains(&fps), "fps must be 1..=240");
                self.config.fps = fps;
                self.apply_config();
                format!("fps {fps}")
            }
            Command::Play => {
                "play mode isn't needed on X11: flurry already sees the real mouse".into()
            }
            Command::Reload => self.reload()?,
            Command::Status => self.status(),
            Command::Quit => return Ok(("quitting".into(), Flow::Quit)),
        };
        Ok((answer, Flow::Continue))
    }

    fn status(&self) -> String {
        format!(
            "{} intensity={} fps={} flakes={} monitors={} panels={} windows={} snow={:.0} \
             on_windows={:.0} flying={}",
            if self.paused { "paused" } else { "running" },
            ipc::intensity_name(self.config.intensity),
            self.config.fps,
            self.world.flakes().len(),
            self.inputs.outputs.len(),
            self.inputs.obstacles.len(),
            self.inputs.windows.len(),
            self.world.piles().volume().max(0.0),
            self.world
                .piles()
                .surfaces()
                .iter()
                .filter(|s| matches!(s.key, crate::sim::piles::SurfaceKey::Window(_)))
                .map(|s| s.volume())
                .sum::<f32>()
                .max(0.0),
            self.world.particles().len()
        )
    }

    fn set_intensity(&mut self, level: Intensity) {
        self.config.intensity = level;
        // An explicit flake_count would override the new intensity.
        self.config.flake_count = None;
        self.apply_config();
    }

    fn reload(&mut self) -> Result<String> {
        let new = Config::load(&self.config_path)?;
        for (changed, what) in [
            (new.monitors != self.config.monitors, "monitors"),
            (new.layering != self.config.layering, "layering"),
            (new.hotkey != self.config.hotkey, "hotkey"),
        ] {
            if changed {
                log::info!("`{what}` changes take effect after restarting flurry");
            }
        }
        self.config = new;
        self.apply_config();
        log::info!("config reloaded from {}", self.config_path.display());
        Ok("config reloaded".into())
    }

    /// Push the current config into the world, the frame pacing and the tray.
    fn apply_config(&mut self) {
        self.world.set_params(SimParams::from_config(&self.config));
        self.frame_interval = frame_interval(self.config.fps);
        self.update_tray();
    }

    fn update_tray(&self) {
        if let Some(tray) = &self.tray {
            tray.update(tray_state(&self.config, self.paused));
        }
    }

    fn set_paused(&mut self, paused: bool) -> Result<()> {
        if paused == self.paused {
            return Ok(());
        }
        self.paused = paused;
        log::info!("{}", if paused { "paused" } else { "resumed" });
        if paused {
            self.hide()?;
        }
        self.update_tray();
        Ok(())
    }

    /// Clear every output; the next frame after resuming redraws in full.
    fn hide(&mut self) -> Result<()> {
        for (i, s) in self.surfaces.iter_mut().enumerate() {
            let all = vec![Rect::new(0, 0, s.rect.w, s.rect.h)];
            s.scene.clear.clone_from(&all);
            s.scene.draws.clear();
            s.pile_scene.clear = all;
            s.pile_scene.piles.clear();
            let frame = Frame {
                flakes: &s.scene,
                piles: &s.pile_scene,
                sprites: self.renderer.sprites(),
            };
            self.backend.present(i, &frame)?;
            s.invalidate();
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        for (i, s) in self.surfaces.iter_mut().enumerate() {
            self.renderer
                .build_scene(&self.world, s.rect, &mut s.damage, &mut s.scene);
            self.renderer
                .build_pile_scene(&self.world, s.rect, &mut s.piles, &mut s.pile_scene);
            let frame = Frame {
                flakes: &s.scene,
                piles: &s.pile_scene,
                sprites: self.renderer.sprites(),
            };
            self.backend.present(i, &frame)?;
        }
        Ok(())
    }

    fn windows_changed(&mut self) {
        self.inputs.windows = self.backend.windows().to_vec();
        self.inputs.hidden_windows = self.backend.hidden_windows().to_vec();
    }

    fn outputs_changed(&mut self) {
        self.inputs.outputs = self.backend.outputs().to_vec();
        self.inputs.obstacles = self.backend.obstacles().to_vec();
        self.windows_changed();
        self.surfaces = surfaces_for(&self.inputs.outputs);
        for o in &self.inputs.outputs {
            log::debug!("output {}: {:?}", o.name, o.rect);
        }
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
