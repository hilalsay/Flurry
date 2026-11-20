//! The daemon: backend + world + renderer + controls + main loop
//! (PLAN.md §4 "Main loop").

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::config::{Config, Intensity};
use crate::geom::{Inputs, OutputInfo, Rect};
use crate::ipc::{self, Command, Request};
use crate::platform::{self, Backend, BackendEvent, BackendKind, BackendOptions, Frame, Hotkey};
use crate::power::{self, PauseReason};
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
/// How often to sample each window's CPU load (hot apps melt).
const HEAT_POLL: Duration = Duration::from_secs(1);
/// How often to check whether we run on battery.
const BATTERY_POLL: Duration = Duration::from_secs(10);
const POWER_SUPPLY: &str = "/sys/class/power_supply";

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
    /// CPU copy of the piles window (created on first draw).
    pile_pixels: Option<tiny_skia::Pixmap>,
}

impl Surface {
    /// The next frame starts from scratch (window content lost).
    fn invalidate(&mut self) {
        self.damage.invalidate();
        self.piles.invalidate();
    }
}

/// Draw the changed parts of `s`'s piles into its CPU pixmap.
fn rasterize<'a>(
    renderer: &Renderer,
    rect: Rect,
    scene: &PileScene,
    pixels: &'a mut Option<tiny_skia::Pixmap>,
) -> Option<&'a tiny_skia::Pixmap> {
    if pixels.as_ref().map(|p| (p.width(), p.height())) != Some((rect.w, rect.h)) {
        *pixels = tiny_skia::Pixmap::new(rect.w, rect.h);
    }
    let p = pixels.as_mut()?;
    if !scene.clear.is_empty() {
        renderer.rasterize_piles(scene, p);
    }
    Some(p)
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
            pile_pixels: None,
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
    let _watchers = power::watch(&tx, true);
    if config.features.privacy_pause {
        crate::privacy::watch_pipewire(tx.clone());
    }
    if config.features.real_weather {
        crate::weather::watch(config.features.weather_city.clone(), tx.clone());
    }
    let _music = if config.features.music_wind {
        crate::music::watch(tx.clone())
            .map_err(|e| log::warn!("music makes wind is off: {e:#}"))
            .ok()
            .map(KillOnDrop)
    } else {
        None
    };
    let tray = Tray::spawn(tx, tray_state(&config, false));
    let mut main = MainLoop::new(config, opts.config_path.clone(), backend.as_mut(), rx, tray);
    let result = main.run();
    log::info!("stopping");
    result
}

/// Stops a helper process when flurry exits.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// How long the main loop may sleep waiting for events: until the next
/// frame while snow shows; while hidden (paused), the longest wait, so a
/// paused flurry sleeps instead of spinning.
fn idle_wait(showing: bool, next_frame: Instant, now: Instant) -> Duration {
    if showing {
        next_frame.saturating_duration_since(now).min(MAX_WAIT)
    } else {
        MAX_WAIT
    }
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
    /// Paused by the user (toggle, tray, hotkey).
    paused: bool,
    /// Paused automatically (sleep, lock, DND, fullscreen).
    auto_paused: BTreeSet<PauseReasonKey>,
    /// What the screen currently shows (false = cleared).
    showing: bool,
    on_battery: bool,
    battery_checked: Option<Instant>,
    heat: crate::heat::Heat,
    heat_sampled: Option<Instant>,
    /// Real weather (feature 5), when known.
    weather: Option<crate::weather::Effect>,
    /// Screen sharing (feature 8): an indicator window / a PipeWire stream.
    sharing_indicator: bool,
    sharing_stream: bool,
    commands: Receiver<Request>,
    tray: Option<Tray>,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    busy: Duration,
    /// Busy time per phase: sim, flake scene, pile scene, pile raster,
    /// present, input regions.
    phases: [Duration; 6],
}

/// `PauseReason` ordered by name, for stable status output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PauseReasonKey(&'static str, u8);

impl From<PauseReason> for PauseReasonKey {
    fn from(r: PauseReason) -> Self {
        PauseReasonKey(r.name(), r as u8)
    }
}

/// The config as it applies right now: real weather sets intensity and
/// wind; on battery, half the flakes and at most 30 fps.
fn effective(config: &Config, on_battery: bool, weather: Option<crate::weather::Effect>) -> Config {
    let mut c = config.clone();
    if let Some(w) = weather {
        c.intensity = w.intensity;
        c.flake_count = None;
        c.wind = config.wind * w.wind;
    }
    if on_battery {
        c.flake_count = Some(c.effective_flake_count() / 2);
        c.fps = c.fps.min(30);
    }
    c
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
            auto_paused: BTreeSet::new(),
            showing: true,
            on_battery: false,
            battery_checked: None,
            heat: crate::heat::Heat::new(),
            heat_sampled: None,
            weather: None,
            sharing_indicator: false,
            sharing_stream: false,
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
            let wait = idle_wait(self.showing, next_frame, Instant::now());
            for event in self.backend.dispatch(wait)? {
                self.handle_event(event)?;
            }
            if let Flow::Quit = self.handle_commands()? {
                return Ok(());
            }
            self.check_battery();
            self.sample_heat();
            let now = Instant::now();
            if !self.showing {
                // Resume without a jump in the simulation.
                (last, next_frame) = (now, now);
                continue;
            }
            if now < next_frame {
                continue;
            }
            acc = (acc + (now - last)).min(MAX_CATCH_UP);
            last = now;
            self.inputs.cursor = if self.config.interaction {
                self.backend.cursor()
            } else {
                None
            };
            let t = Instant::now();
            while acc >= STEP {
                self.world.step(STEP.as_secs_f32(), &self.inputs);
                // Key presses count once, in the first step that sees them.
                self.inputs.keys_pressed = 0;
                acc -= STEP;
            }
            self.stats.phases[0] += t.elapsed();
            self.draw()?;
            let t = Instant::now();
            self.update_grab_regions()?;
            self.stats.phases[5] += t.elapsed();
            self.record(now);
            next_frame += self.frame_interval;
            if next_frame < now {
                // Fell behind: don't burst frames to catch up.
                next_frame = now + self.frame_interval;
            }
        }
    }

    fn handle_event(&mut self, event: BackendEvent) -> Result<()> {
        match event {
            BackendEvent::OutputsChanged => self.outputs_changed(),
            BackendEvent::Hotkey => {
                self.paused = !self.paused;
                self.update_visibility()?;
            }
            BackendEvent::ObstaclesChanged => {
                self.inputs.obstacles = self.backend.obstacles().to_vec();
            }
            BackendEvent::WindowsChanged => {
                self.windows_changed();
                self.inputs.active_window = self.backend.active_window();
                let privacy = self.config.features.privacy_pause;
                let fs = (self.config.auto_pause_fullscreen && self.backend.fullscreen())
                    || (privacy && self.backend.any_fullscreen());
                self.set_auto(PauseReason::Fullscreen, fs)?;
                let indicator = privacy && self.backend.sharing_indicator();
                if indicator != self.sharing_indicator {
                    self.sharing_indicator = indicator;
                    self.update_sharing()?;
                }
            }
            BackendEvent::Redraw(i) => {
                if let Some(s) = self.surfaces.get_mut(i) {
                    s.invalidate();
                }
            }
            BackendEvent::Keys(n) => {
                self.inputs.keys_pressed = self.inputs.keys_pressed.saturating_add(n);
            }
        }
        Ok(())
    }

    fn set_auto(&mut self, reason: PauseReason, on: bool) -> Result<()> {
        let key = PauseReasonKey::from(reason);
        let changed = if on {
            self.auto_paused.insert(key)
        } else {
            self.auto_paused.remove(&key)
        };
        if changed {
            log::info!(
                "auto-pause {}: {}",
                reason.name(),
                if on { "on" } else { "off" }
            );
            self.update_visibility()?;
        }
        Ok(())
    }

    /// Feature "hot apps melt": CPU load of each visible window's program.
    fn sample_heat(&mut self) {
        let wanted = self.config.features.hot_apps_melt
            && self.showing
            && self.inputs.windows.iter().any(|w| w.collects);
        if !wanted || self.heat_sampled.is_some_and(|t| t.elapsed() < HEAT_POLL) {
            return;
        }
        self.heat_sampled = Some(Instant::now());
        self.heat.sample();
        self.inputs.window_heat = self
            .inputs
            .windows
            .iter()
            .filter_map(|w| Some((w.id, self.heat.of(w.pid?))))
            .collect();
    }

    fn update_sharing(&mut self) -> Result<()> {
        let sharing = self.sharing_indicator || self.sharing_stream;
        self.set_auto(PauseReason::ScreenShare, sharing)
    }

    fn check_battery(&mut self) {
        if self
            .battery_checked
            .is_some_and(|t| t.elapsed() < BATTERY_POLL)
        {
            return;
        }
        self.battery_checked = Some(Instant::now());
        let on = power::on_battery(Path::new(POWER_SUPPLY));
        if on != self.on_battery {
            self.on_battery = on;
            log::info!(
                "{}",
                if on {
                    "on battery: half the flakes, at most 30 fps"
                } else {
                    "on mains power"
                }
            );
            self.apply_config();
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
                self.paused = !self.paused;
                self.update_visibility()?;
                match (self.paused, self.auto_reasons()) {
                    (true, _) => "paused".to_owned(),
                    (false, None) => "resumed".to_owned(),
                    (false, Some(why)) => format!("resumed (still paused while: {why})"),
                }
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
            Command::Tornado => {
                if self.showing {
                    self.world.start_storm();
                    log::info!("a tornado is coming");
                    format!(
                        "tornado! a snow storm for {:.0} s",
                        crate::sim::tornado::DURATION
                    )
                } else {
                    "paused: resume first (flurry toggle)".into()
                }
            }
            Command::Reload => self.reload()?,
            Command::Status => self.status(),
            Command::Quit => return Ok(("quitting".into(), Flow::Quit)),
            Command::Auto(PauseReason::ScreenShare, on) => {
                self.sharing_stream = on;
                self.update_sharing()?;
                "ok".into()
            }
            Command::Auto(reason, on) => {
                self.set_auto(reason, on)?;
                "ok".into()
            }
            Command::Gust(strength) => {
                if self.showing {
                    self.world.gust(strength);
                }
                "ok".into()
            }
            Command::Weather(e) => {
                self.weather = Some(e);
                self.apply_config();
                "ok".into()
            }
            Command::Wake => {
                for event in self.backend.rescan()? {
                    self.handle_event(event)?;
                }
                "ok".into()
            }
        };
        Ok((answer, Flow::Continue))
    }

    fn status(&self) -> String {
        format!(
            "{} intensity={} fps={} flakes={} monitors={} panels={} windows={} snow={:.0} \
             on_windows={:.0} flying={} snowballs={} snowmen={}{}{}{}",
            match (self.paused, self.auto_reasons()) {
                (true, _) => "paused".to_owned(),
                (false, None) => "running".to_owned(),
                (false, Some(why)) => format!("auto-paused({why})"),
            },
            ipc::intensity_name(effective(&self.config, self.on_battery, self.weather).intensity),
            effective(&self.config, self.on_battery, self.weather).fps,
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
            self.world.particles().len(),
            self.world.snowfolk().balls().len(),
            self.world.snowfolk().snowmen().len(),
            if self.on_battery { " battery" } else { "" },
            self.world
                .storm_remaining()
                .map(|s| format!(" storm={s:.0}s"))
                .unwrap_or_default(),
            self.weather
                .map(|w| format!(" weather_wind={:.2}", w.wind))
                .unwrap_or_default()
        )
    }

    /// The active automatic pause reasons, comma-separated.
    fn auto_reasons(&self) -> Option<String> {
        (!self.auto_paused.is_empty()).then(|| {
            self.auto_paused
                .iter()
                .map(|k| k.0)
                .collect::<Vec<_>>()
                .join(",")
        })
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
        let config = effective(&self.config, self.on_battery, self.weather);
        self.world.set_params(SimParams::from_config(&config));
        self.frame_interval = frame_interval(config.fps);
        self.update_tray();
    }

    fn update_tray(&self) {
        if let Some(tray) = &self.tray {
            tray.update(tray_state(&self.config, self.paused));
        }
    }

    /// Show or hide the snow after the user or an automatic reason changed.
    fn update_visibility(&mut self) -> Result<()> {
        let show = !self.paused && self.auto_paused.is_empty();
        if show != self.showing {
            self.showing = show;
            log::info!("{}", if show { "snow resumed" } else { "snow paused" });
            if !show {
                self.hide()?;
            }
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
            s.pile_scene.draws.clear();
            s.pile_scene.images.clear();
            s.pile_scene.icicles.clear();
            s.pile_scene.frost.clear();
            let pixels = rasterize(&self.renderer, s.rect, &s.pile_scene, &mut s.pile_pixels);
            let frame = Frame {
                flakes: &s.scene,
                piles: &s.pile_scene,
                sprites: self.renderer.sprites(),
                pile_pixels: pixels,
            };
            self.backend.present(i, &frame)?;
            s.invalidate();
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let ph = &mut self.stats.phases;
        for (i, s) in self.surfaces.iter_mut().enumerate() {
            let t = Instant::now();
            self.renderer
                .build_scene(&self.world, s.rect, &mut s.damage, &mut s.scene);
            let t1 = Instant::now();
            self.renderer
                .build_pile_scene(&self.world, s.rect, &mut s.piles, &mut s.pile_scene);
            let t2 = Instant::now();
            let pixels = rasterize(&self.renderer, s.rect, &s.pile_scene, &mut s.pile_pixels);
            let t3 = Instant::now();
            ph[1] += t1 - t;
            ph[2] += t2 - t1;
            ph[3] += t3 - t2;
            let frame = Frame {
                flakes: &s.scene,
                piles: &s.pile_scene,
                sprites: self.renderer.sprites(),
                pile_pixels: pixels,
            };
            self.backend.present(i, &frame)?;
            ph[4] += t3.elapsed();
        }
        Ok(())
    }

    fn windows_changed(&mut self) {
        self.inputs.windows = self.backend.windows().to_vec();
        self.inputs.hidden_windows = self.backend.hidden_windows().to_vec();
    }

    /// Let clicks on snowballs reach flurry (to pick them up) while the
    /// carry modifier is held; everywhere else stays click-through.
    fn update_grab_regions(&mut self) -> Result<()> {
        let grab = self.world.grab_rects(self.inputs.cursor.as_ref());
        for (i, s) in self.surfaces.iter().enumerate() {
            let local: Vec<Rect> = grab
                .iter()
                .map(|r| Rect::new(r.x - s.rect.x, r.y - s.rect.y, r.w, r.h))
                .filter_map(|r| r.intersect(&Rect::new(0, 0, s.rect.w, s.rect.h)))
                .collect();
            self.backend.set_input_region(i, &local)?;
        }
        Ok(())
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
            let n = s.frames.max(1);
            let [sim, flakes, piles, raster, present, input] = s.phases.map(|d| d / n);
            log::debug!(
                "{} frames in {:.1?}, avg {:?} busy per frame, {} flakes \
                 (sim {sim:?}, flakes {flakes:?}, piles {piles:?}, raster {raster:?}, \
                 present {present:?}, input {input:?})",
                s.frames,
                since.elapsed(),
                s.busy / n,
                self.world.flakes().len()
            );
            *s = Stats::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_main_loop_sleeps_instead_of_spinning() {
        let now = Instant::now();
        // Hidden: the loop resets next_frame to now each pass; it must still wait.
        assert_eq!(idle_wait(false, now, now), MAX_WAIT);
        assert_eq!(idle_wait(true, now, now), Duration::ZERO);
        let soon = now + Duration::from_millis(20);
        assert_eq!(idle_wait(true, soon, now), Duration::from_millis(20));
        assert_eq!(idle_wait(true, now + Duration::from_secs(5), now), MAX_WAIT);
    }
}
