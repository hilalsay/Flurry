//! Wayland backend: two wlr-layer-shell surfaces per monitor, like the two
//! X11 windows. Falling flakes go on the `Bottom` layer (above the
//! wallpaper, below app windows); settled snow goes on the `Top` layer
//! (above windows, below fullscreen ones and the lock screen). Both are
//! click-through except where `set_input_region` asks for clicks
//! (snowballs).
//!
//! Drawing is on the CPU into shared-memory buffers. Flakes are drawn
//! straight into a reused buffer: it is cleared wherever any frame it
//! missed changed something, then every flake is drawn (see
//! [`render::replay_bgra`]); no full-screen copy is kept. Piles are copied
//! from the CPU raster the app keeps. Each surface commits at most once per
//! compositor frame callback.
//!
//! Window positions, panels and (on Hyprland and KDE) the pointer come from
//! a compositor-specific provider, see [`super::wininfo`].

use std::os::fd::AsFd;
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_dispatch2, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler, BTN_LEFT},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, EventQueue, QueueHandle,
};

use super::wininfo::{self, Snapshot};
use super::{filter_outputs, Backend, BackendEvent, BackendOptions, Frame};
use crate::config::Monitors;
use crate::geom::{AppWindow, CursorState, OutputInfo, Rect, WindowRect};
use crate::render::{copy_rgba_to_bgra, replay_bgra};

/// Buffers per surface: one shown, one being drawn, one spare.
const MAX_BUFFERS: usize = 3;
/// Past this many rects a buffer has missed, redraw it whole.
const MAX_MISSED: usize = 512;

/// One layer surface and its buffers.
struct Surface {
    layer: LayerSurface,
    pool: Option<SlotPool>,
    buffers: Vec<Buf>,
    /// Configured size; `None` until the first configure.
    size: Option<(u32, u32)>,
    /// A frame callback is outstanding: don't commit yet.
    waiting: bool,
    /// Changed since the last commit (what to `damage_buffer`).
    uncommitted: Vec<Rect>,
}

struct Buf {
    buffer: Buffer,
    /// What changed since this buffer was last brought up to date.
    missed: Vec<Rect>,
}

impl Surface {
    fn new(
        state: &State,
        qh: &QueueHandle<State>,
        output: &wl_output::WlOutput,
        layer: Layer,
        ns: &str,
    ) -> Result<Surface> {
        let surface = state.compositor.create_surface(qh);
        let layer =
            state
                .layer_shell
                .create_layer_surface(qh, surface, layer, Some(ns), Some(output));
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        let empty = Region::new(&state.compositor)?;
        layer.wl_surface().set_input_region(Some(empty.wl_region()));
        layer.commit();
        Ok(Surface {
            layer,
            pool: None,
            buffers: Vec::new(),
            size: None,
            waiting: false,
            uncommitted: Vec::new(),
        })
    }

    /// Record that `rects` changed (in every buffer, and since the last commit).
    fn changed(&mut self, rects: &[Rect]) {
        let Some((w, h)) = self.size else { return };
        let whole = Rect::new(0, 0, w, h);
        for list in self
            .buffers
            .iter_mut()
            .map(|b| &mut b.missed)
            .chain(std::iter::once(&mut self.uncommitted))
        {
            if list.len() + rects.len() > MAX_MISSED {
                list.clear();
                list.push(whole);
            } else {
                list.extend(rects.iter().filter_map(|r| r.intersect(&whole)));
            }
        }
    }

    /// A buffer the compositor isn't using, creating one if needed.
    fn free_buffer(&mut self, shm: &Shm) -> Option<usize> {
        let (w, h) = self.size?;
        let pool = match &mut self.pool {
            Some(p) => p,
            None => self
                .pool
                .insert(SlotPool::new((w * h * 4) as usize * 2, shm).ok()?),
        };
        if let Some(i) = self
            .buffers
            .iter()
            .position(|b| b.buffer.canvas(pool).is_some())
        {
            return Some(i);
        }
        if self.buffers.len() >= MAX_BUFFERS {
            return None;
        }
        let (buffer, _) = pool
            .create_buffer(w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888)
            .ok()?;
        // Fresh memory: draw all of it.
        self.buffers.push(Buf {
            buffer,
            missed: vec![Rect::new(0, 0, w, h)],
        });
        Some(self.buffers.len() - 1)
    }

    /// Attach buffer `i`, damage what changed, ask for a frame callback, commit.
    fn commit(&mut self, i: usize, qh: &QueueHandle<State>) {
        let surface = self.layer.wl_surface();
        if self.buffers[i].buffer.attach_to(surface).is_err() {
            return;
        }
        for r in self.uncommitted.drain(..) {
            surface.damage_buffer(r.x, r.y, r.w as i32, r.h as i32);
        }
        surface.frame(qh, FrameCallbackData(surface.clone()));
        self.waiting = true;
        self.layer.commit();
    }
}

/// The surfaces of one monitor.
struct Screen {
    info: OutputInfo,
    flakes: Surface,
    piles: Surface,
}

struct State {
    registry: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    seat: SeatState,
    pointer: Option<wl_pointer::WlPointer>,
    screens: Vec<Screen>,
    /// Monitors were added, removed or changed since the last check.
    outputs_dirty: bool,
    /// The pointer over one of our surfaces: (screen, x, y), and the left button.
    pointer_at: Option<(usize, f64, f64)>,
    left_down: bool,
}

impl State {
    fn screen_of(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.screens.iter().position(|s| {
            s.flakes.layer.wl_surface() == surface || s.piles.layer.wl_surface() == surface
        })
    }

    fn surface_mut(&mut self, surface: &wl_surface::WlSurface) -> Option<&mut Surface> {
        self.screens.iter_mut().find_map(|s| {
            if s.flakes.layer.wl_surface() == surface {
                Some(&mut s.flakes)
            } else if s.piles.layer.wl_surface() == surface {
                Some(&mut s.piles)
            } else {
                None
            }
        })
    }

    /// The monitors as flurry sees them: logical position and size.
    fn output_infos(&self) -> Vec<(wl_output::WlOutput, OutputInfo)> {
        self.output_state
            .outputs()
            .filter_map(|o| {
                let i = self.output_state.info(&o)?;
                let (x, y) = i.logical_position.unwrap_or(i.location);
                let (w, h) = i.logical_size.or_else(|| {
                    let m = i.modes.iter().find(|m| m.current)?;
                    let s = i.scale_factor.max(1);
                    Some((m.dimensions.0 / s, m.dimensions.1 / s))
                })?;
                (w > 0 && h > 0).then(|| {
                    let name = i.name.clone().unwrap_or_else(|| format!("WL-{}", i.id));
                    let info = OutputInfo {
                        name,
                        rect: Rect::new(x, y, w as u32, h as u32),
                        scale: i.scale_factor as f32,
                    };
                    (o, info)
                })
            })
            .collect()
    }
}

pub struct WaylandBackend {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    monitors: Monitors,
    outputs: Vec<OutputInfo>,
    provider: wininfo::Provider,
    snapshot: Snapshot,
    snapshot_version: u64,
    obstacles: Vec<WindowRect>,
}

impl WaylandBackend {
    pub fn new(opts: &BackendOptions) -> Result<Self> {
        let conn = Connection::connect_to_env().context("cannot connect to the Wayland display")?;
        let (globals, mut queue) = registry_queue_init(&conn)?;
        let qh = queue.handle();
        let layer_shell = LayerShell::bind(&globals, &qh).context(
            "this compositor has no wlr-layer-shell (GNOME doesn't); use the X11 backend",
        )?;
        let mut state = State {
            registry: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            compositor: CompositorState::bind(&globals, &qh).context("no wl_compositor")?,
            layer_shell,
            shm: Shm::bind(&globals, &qh).context("no wl_shm")?,
            seat: SeatState::new(&globals, &qh),
            pointer: None,
            screens: Vec::new(),
            outputs_dirty: true,
            pointer_at: None,
            left_down: false,
        };
        // Outputs and their names/positions arrive over a few round trips.
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;
        if opts.hotkey.is_some() {
            log::info!(
                "the toggle hotkey isn't available on Wayland; bind `flurry toggle` to a key in \
                 your desktop's keyboard settings"
            );
        }
        let mut me = WaylandBackend {
            conn,
            queue,
            state,
            monitors: opts.monitors.clone(),
            outputs: Vec::new(),
            provider: wininfo::start(),
            snapshot: Snapshot::default(),
            snapshot_version: u64::MAX,
            obstacles: Vec::new(),
        };
        me.rebuild()?;
        me.pull_snapshot();
        me.queue.roundtrip(&mut me.state)?;
        log::info!(
            "Wayland: {} monitor(s), window info from {:?}",
            me.outputs.len(),
            me.provider.kind
        );
        Ok(me)
    }

    /// Recreate the surfaces for the current monitors.
    fn rebuild(&mut self) -> Result<()> {
        self.state.outputs_dirty = false;
        let all = self.state.output_infos();
        let chosen = filter_outputs(all.iter().map(|(_, i)| i.clone()).collect(), &self.monitors);
        let qh = self.queue.handle();
        self.state.screens.clear();
        self.state.pointer_at = None;
        for info in &chosen {
            let Some((output, _)) = all.iter().find(|(_, i)| i == info) else {
                continue;
            };
            let flakes = Surface::new(&self.state, &qh, output, Layer::Bottom, "flurry-flakes")?;
            let piles = Surface::new(&self.state, &qh, output, Layer::Top, "flurry-piles")?;
            self.state.screens.push(Screen {
                info: info.clone(),
                flakes,
                piles,
            });
        }
        self.outputs = chosen;
        self.update_obstacles();
        Ok(())
    }

    /// Panels: the parts of each monitor outside its work area.
    fn update_obstacles(&mut self) {
        let mut out = Vec::new();
        for o in &self.outputs {
            let r = o.rect;
            let Some(a) = self
                .snapshot
                .work_areas
                .iter()
                .filter_map(|a| a.intersect(&r))
                .max_by_key(|a| u64::from(a.w) * u64::from(a.h))
            else {
                continue;
            };
            let strips = [
                Rect::new(r.x, r.y, r.w, (a.y - r.y).max(0) as u32),
                Rect::new(
                    r.x,
                    a.bottom(),
                    r.w,
                    (r.bottom() - a.bottom()).max(0) as u32,
                ),
                Rect::new(r.x, a.y, (a.x - r.x).max(0) as u32, a.h),
                Rect::new(a.right(), a.y, (r.right() - a.right()).max(0) as u32, a.h),
            ];
            for s in strips.into_iter().filter(|s| !s.is_empty()) {
                out.push(WindowRect {
                    id: 0xF1_0000 + out.len() as u64,
                    rect: s,
                });
            }
        }
        self.obstacles = out;
    }

    /// Take the provider's latest snapshot; true if it changed.
    fn pull_snapshot(&mut self) -> bool {
        let (s, v) = self.provider.shared.get();
        if v == self.snapshot_version {
            return false;
        }
        self.snapshot_version = v;
        let areas_changed = s.work_areas != self.snapshot.work_areas;
        self.snapshot = s;
        if areas_changed {
            self.update_obstacles();
        }
        true
    }

    fn wait_readable(&mut self, timeout: Duration) -> Result<()> {
        self.queue.flush()?;
        self.queue.dispatch_pending(&mut self.state)?;
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(());
        };
        let ts = Timespec::try_from(timeout).unwrap_or(Timespec {
            tv_sec: i64::MAX,
            tv_nsec: 0,
        });
        let ready = {
            let fd = guard.connection_fd();
            let mut fds = [PollFd::from_borrowed_fd(fd.as_fd(), PollFlags::IN)];
            match rustix::event::poll(&mut fds, Some(&ts)) {
                Ok(n) => n > 0,
                Err(rustix::io::Errno::INTR) => false,
                Err(e) => return Err(e.into()),
            }
        };
        if ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.into()),
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

impl Backend for WaylandBackend {
    fn name(&self) -> &'static str {
        "wayland"
    }

    fn outputs(&self) -> &[OutputInfo] {
        &self.outputs
    }

    fn obstacles(&self) -> &[WindowRect] {
        &self.obstacles
    }

    fn windows(&self) -> &[AppWindow] {
        &self.snapshot.windows
    }

    fn hidden_windows(&self) -> &[u64] {
        &self.snapshot.hidden
    }

    fn fullscreen(&self) -> bool {
        self.snapshot.fullscreen
    }

    fn active_window(&self) -> Option<u64> {
        self.snapshot.active
    }

    fn any_fullscreen(&self) -> bool {
        self.snapshot.any_fullscreen
    }

    fn sharing_indicator(&self) -> bool {
        false
    }

    fn rescan(&mut self) -> Result<Vec<BackendEvent>> {
        self.queue.roundtrip(&mut self.state)?;
        self.rebuild()?;
        self.pull_snapshot();
        Ok(vec![
            BackendEvent::OutputsChanged,
            BackendEvent::ObstaclesChanged,
            BackendEvent::WindowsChanged,
        ])
    }

    fn cursor(&mut self) -> Option<CursorState> {
        // Over our own clickable areas we see the pointer and its button.
        if let Some((i, x, y)) = self.state.pointer_at {
            let o = self.state.screens.get(i)?.info.rect;
            return Some(CursorState {
                x: o.x as f32 + x as f32,
                y: o.y as f32 + y as f32,
                left_down: self.state.left_down,
                ..CursorState::default()
            });
        }
        let (x, y) = self.provider.shared.cursor()?;
        Some(CursorState {
            x,
            y,
            ..CursorState::default()
        })
    }

    fn set_input_region(&mut self, output: usize, rects: &[Rect]) -> Result<()> {
        let Some(s) = self.state.screens.get_mut(output) else {
            return Ok(());
        };
        let region = Region::new(&self.state.compositor)?;
        for r in rects {
            region.add(r.x, r.y, r.w as i32, r.h as i32);
        }
        let surface = s.piles.layer.wl_surface();
        surface.set_input_region(Some(region.wl_region()));
        if !s.piles.waiting {
            // Input regions apply on commit; frames commit anyway.
            surface.commit();
        }
        Ok(())
    }

    fn present(&mut self, output: usize, frame: &Frame) -> Result<()> {
        let qh = self.queue.handle();
        let shm = &self.state.shm;
        let Some(screen) = self.state.screens.get_mut(output) else {
            return Ok(());
        };
        // Flakes: clear what any missed frame touched, draw every flake.
        let f = &mut screen.flakes;
        f.changed(&frame.flakes.clear);
        if f.size.is_some() && !f.waiting && !f.uncommitted.is_empty() {
            if let Some(i) = f.free_buffer(shm) {
                let (w, h) = f.size.unwrap_or_default();
                let pool = f.pool.as_mut().context("pool")?;
                let b = &mut f.buffers[i];
                if let Some(canvas) = b.buffer.canvas(pool) {
                    replay_bgra(
                        &[&b.missed],
                        &frame.flakes.draws,
                        frame.sprites,
                        canvas,
                        w,
                        h,
                    );
                    b.missed.clear();
                    f.commit(i, &qh);
                }
            }
        }
        // Piles: copy what changed from the CPU raster.
        let p = &mut screen.piles;
        p.changed(&frame.piles.clear);
        if let Some(src) = frame.pile_pixels {
            if p.size.is_some() && !p.waiting && !p.uncommitted.is_empty() {
                if let Some(i) = p.free_buffer(shm) {
                    let (w, h) = p.size.unwrap_or_default();
                    let pool = p.pool.as_mut().context("pool")?;
                    let b = &mut p.buffers[i];
                    if let Some(canvas) = b.buffer.canvas(pool) {
                        copy_rects(src, canvas, w, h, &b.missed);
                        b.missed.clear();
                        p.commit(i, &qh);
                    }
                }
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn dispatch(&mut self, timeout: Duration) -> Result<Vec<BackendEvent>> {
        self.wait_readable(timeout)?;
        let mut events = Vec::new();
        if self.state.outputs_dirty {
            let before = self.outputs.clone();
            let now: Vec<OutputInfo> = filter_outputs(
                self.state
                    .output_infos()
                    .into_iter()
                    .map(|(_, i)| i)
                    .collect(),
                &self.monitors,
            );
            self.state.outputs_dirty = false;
            if now != before {
                self.rebuild()?;
                events.push(BackendEvent::OutputsChanged);
            }
        }
        let areas = self.snapshot.work_areas.clone();
        if self.pull_snapshot() {
            events.push(BackendEvent::WindowsChanged);
            if self.snapshot.work_areas != areas {
                events.push(BackendEvent::ObstaclesChanged);
            }
        }
        Ok(events)
    }
}

/// Copy `rects` of `src` (premultiplied RGBA, output-sized) into `dst`
/// (premultiplied BGRA, `w`×`h`), clipped to both.
fn copy_rects(src: &tiny_skia::Pixmap, dst: &mut [u8], w: u32, h: u32, rects: &[Rect]) {
    let bounds = Rect::new(0, 0, w.min(src.width()), h.min(src.height()));
    let (ss, ds) = (src.width() as usize * 4, w as usize * 4);
    let data = src.data();
    for r in rects.iter().filter_map(|r| r.intersect(&bounds)) {
        let (x0, x1) = (r.x as usize * 4, r.right() as usize * 4);
        for y in r.y as usize..r.bottom() as usize {
            copy_rgba_to_bgra(
                &data[y * ss + x0..y * ss + x1],
                &mut dst[y * ds + x0..y * ds + x1],
            );
        }
    }
}

// ----- protocol handlers --------------------------------------------------

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(s) = self.surface_mut(surface) {
            s.waiting = false;
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.outputs_dirty = true;
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.outputs_dirty = true;
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.outputs_dirty = true;
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor took the surface away (e.g. its monitor went):
        // rebuild everything.
        log::debug!("layer surface closed: {:?}", layer.wl_surface());
        self.outputs_dirty = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(s) = self.surface_mut(layer.wl_surface()) else {
            return;
        };
        let size = (configure.new_size.0.max(1), configure.new_size.1.max(1));
        if s.size != Some(size) {
            log::debug!("layer surface configured {}x{}", size.0, size.1);
            s.size = Some(size);
            s.buffers.clear();
            s.pool = None;
            s.waiting = false;
            s.uncommitted = vec![Rect::new(0, 0, size.0, size.1)];
        }
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
            self.pointer_at = None;
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for e in events {
            let Some(screen) = self.screen_of(&e.surface) else {
                continue;
            };
            match e.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.pointer_at = Some((screen, e.position.0, e.position.1));
                }
                PointerEventKind::Leave { .. } => {
                    // Keep a carried snowball: the button stays down until released.
                    if !self.left_down {
                        self.pointer_at = None;
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.left_down = true;
                    self.pointer_at = Some((screen, e.position.0, e.position.1));
                }
                PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                    self.left_down = false;
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(State);
delegate_dispatch2!(State);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_rects_swaps_channels_only_inside() {
        let mut src = tiny_skia::Pixmap::new(4, 2).unwrap();
        src.data_mut()
            .copy_from_slice(&[[10u8, 20, 30, 255]; 8].concat());
        let mut dst = vec![0u8; 4 * 2 * 4];
        copy_rects(&src, &mut dst, 4, 2, &[Rect::new(1, 1, 2, 5)]);
        let px = |x: usize, y: usize| &dst[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(1, 1), [30, 20, 10, 255]);
        assert_eq!(px(2, 1), [30, 20, 10, 255]);
        assert_eq!(px(0, 1), [0, 0, 0, 0]);
        assert_eq!(px(1, 0), [0, 0, 0, 0]);
    }
}
