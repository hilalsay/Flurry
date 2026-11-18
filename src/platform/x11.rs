//! X11 overlay: one ARGB override-redirect window per monitor with an empty
//! input shape (click-through), drawn server-side with XRender.
//!
//! The flake sprites are uploaded once as an A8 glyph set. Each frame is then
//! one `FillRectangles` (clear the damage) plus one `CompositeGlyphs` per
//! depth layer (solid layer colour through the glyph masks). No pixels cross
//! the wire, and with glamor the server does it all on the GPU. This measured
//! far cheaper than MIT-SHM `PutImage` of damaged rects: see PLAN.md changelog.

use std::os::fd::AsFd;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::render::{
    self, ConnectionExt as _, CreatePictureAux, Glyphinfo, PictOp, PictType, Pictformat,
};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClipOrdering, ColormapAlloc, ConfigureWindowAux,
    ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Rectangle, Screen, StackMode,
    VisualClass, Visualid, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{filter_outputs, Backend, BackendEvent, BackendOptions};
use crate::geom::{OutputInfo, Rect};
use crate::render::{layer_rgb, Scene, SpriteDraw, SpriteSheet};
use crate::sim::flakes::Layer;

pub const NO_COMPOSITOR_MSG: &str = "no compositing manager is running on this X screen, \
so the snow overlay cannot be transparent.\n\
Start one (for example `picom &`) or turn on compositing in your desktop settings, \
or pass --no-compositor-check to run anyway.";

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING,
        _NET_WM_NAME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DND,
        _NET_WM_BYPASS_COMPOSITOR,
    }
}

/// Bytes per CompositeGlyphs32 element holding one glyph.
const GLYPH_ELT_BYTES: usize = 12;

struct Surface {
    window: Window,
    picture: render::Picture,
    w: u16,
    h: u16,
    /// Window content is undefined (just mapped or exposed): redraw it all.
    full_pending: bool,
}

/// Server-side copies of the sprites, created on first present.
struct Glyphs {
    set: render::Glyphset,
    /// Solid-fill source pictures, one per layer.
    colors: [render::Picture; 3],
}

pub struct X11Backend {
    conn: RustConnection,
    root: Window,
    visual: Visualid,
    colormap: u32,
    /// XRender format of the ARGB visual, and the A8 format for glyphs.
    argb_format: Pictformat,
    a8_format: Pictformat,
    atoms: Atoms,
    has_randr: bool,
    monitors: crate::config::Monitors,
    outputs: Vec<OutputInfo>,
    surfaces: Vec<Surface>,
    glyphs: Option<Glyphs>,
    /// Reused encoding buffers.
    cmds: Vec<u8>,
    rects: Vec<Rectangle>,
}

impl X11Backend {
    pub fn new(opts: &BackendOptions) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .context("cannot connect to the X server (is DISPLAY set and the server running?)")?;
        let screen = conn.setup().roots[screen_num].clone();
        log::debug!(
            "X11: screen {screen_num}, {}x{}, vendor {:?}",
            screen.width_in_pixels,
            screen.height_in_pixels,
            String::from_utf8_lossy(&conn.setup().vendor)
        );
        for (ext, why) in [
            (shape::X11_EXTENSION_NAME, "click-through"),
            (render::X11_EXTENSION_NAME, "drawing"),
        ] {
            if conn.extension_information(ext)?.is_none() {
                bail!("the X server lacks the {ext} extension, which {why} needs");
            }
        }
        check_compositor(&conn, screen_num, opts.no_compositor_check)?;
        let visual = find_argb_visual(&screen)?;
        let (argb_format, a8_format) = find_formats(&conn, visual)?;
        let has_randr = randr_usable(&conn)?;

        let colormap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;
        let atoms = Atoms::new(&conn)?.reply()?;

        // Root events: other windows being restacked/mapped (to stay on top).
        conn.change_window_attributes(
            screen.root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()
        .context("cannot listen for window changes on the root window")?;
        if has_randr {
            conn.randr_select_input(
                screen.root,
                randr::NotifyMask::SCREEN_CHANGE
                    | randr::NotifyMask::CRTC_CHANGE
                    | randr::NotifyMask::OUTPUT_CHANGE,
            )?;
        }

        let mut backend = X11Backend {
            conn,
            root: screen.root,
            visual,
            colormap,
            argb_format,
            a8_format,
            atoms,
            has_randr,
            monitors: opts.monitors.clone(),
            outputs: Vec::new(),
            surfaces: Vec::new(),
            glyphs: None,
            cmds: Vec::new(),
            rects: Vec::new(),
        };
        backend.outputs = backend.query_outputs(&screen)?;
        backend.create_surfaces()?;
        Ok(backend)
    }

    fn screen(&self) -> Screen {
        let roots = &self.conn.setup().roots;
        roots
            .iter()
            .find(|s| s.root == self.root)
            .unwrap_or(&roots[0])
            .clone()
    }

    fn query_outputs(&self, screen: &Screen) -> Result<Vec<OutputInfo>> {
        let mut outputs = if self.has_randr {
            self.randr_outputs()?
        } else {
            Vec::new()
        };
        if outputs.is_empty() {
            outputs.push(OutputInfo {
                name: "X11-SCREEN".into(),
                rect: Rect::new(
                    0,
                    0,
                    screen.width_in_pixels.into(),
                    screen.height_in_pixels.into(),
                ),
                scale: 1.0,
            });
        }
        let outputs = filter_outputs(outputs, &self.monitors);
        for o in &outputs {
            log::debug!("X11 output {}: {:?}", o.name, o.rect);
        }
        Ok(outputs)
    }

    fn randr_outputs(&self) -> Result<Vec<OutputInfo>> {
        let res = self
            .conn
            .randr_get_screen_resources_current(self.root)?
            .reply()?;
        let mut outputs: Vec<OutputInfo> = Vec::new();
        for &crtc in &res.crtcs {
            let info = self
                .conn
                .randr_get_crtc_info(crtc, res.config_timestamp)?
                .reply()?;
            let Some(&first) = info.outputs.first() else {
                continue;
            };
            if info.mode == 0 || info.width == 0 || info.height == 0 {
                continue;
            }
            let rect = Rect::new(
                info.x.into(),
                info.y.into(),
                info.width.into(),
                info.height.into(),
            );
            // Mirrored outputs share a rect: one overlay is enough.
            if outputs.iter().any(|o| o.rect == rect) {
                continue;
            }
            let out = self
                .conn
                .randr_get_output_info(first, res.config_timestamp)?
                .reply()?;
            outputs.push(OutputInfo {
                name: String::from_utf8_lossy(&out.name).into_owned(),
                rect,
                scale: 1.0,
            });
        }
        Ok(outputs)
    }

    fn create_surfaces(&mut self) -> Result<()> {
        let rects: Vec<Rect> = self.outputs.iter().map(|o| o.rect).collect();
        for rect in rects {
            let s = self.create_surface(rect)?;
            self.surfaces.push(s);
        }
        self.conn.flush()?;
        Ok(())
    }

    fn create_surface(&mut self, rect: Rect) -> Result<Surface> {
        let (w, h) = (
            u16::try_from(rect.w).context("output too wide")?,
            u16::try_from(rect.h).context("output too tall")?,
        );
        let window = self.conn.generate_id()?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .override_redirect(1)
            .colormap(self.colormap)
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY);
        self.conn.create_window(
            32,
            window,
            self.root,
            rect.x as i16,
            rect.y as i16,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            self.visual,
            &aux,
        )?;
        // Click-through: an empty input region.
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            window,
            0,
            0,
            &[],
        )?;
        self.set_properties(window)?;
        let picture = self.conn.generate_id()?;
        self.conn.render_create_picture(
            picture,
            window,
            self.argb_format,
            &CreatePictureAux::new(),
        )?;
        self.conn.map_window(window)?;
        self.raise(window)?;
        Ok(Surface {
            window,
            picture,
            w,
            h,
            full_pending: true,
        })
    }

    fn set_properties(&self, window: Window) -> Result<()> {
        let c = &self.conn;
        let a = &self.atoms;
        c.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"flurry",
        )?;
        c.change_property8(
            PropMode::REPLACE,
            window,
            a._NET_WM_NAME,
            a.UTF8_STRING,
            b"flurry",
        )?;
        c.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"flurry\0Flurry\0",
        )?;
        // DND windows get no shadow in picom's default config; Mutter/Muffin
        // don't shadow frameless ARGB windows anyway.
        c.change_property32(
            PropMode::REPLACE,
            window,
            a._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[a._NET_WM_WINDOW_TYPE_DND],
        )?;
        // 2 = "please don't unredirect me" (keeps transparency working).
        c.change_property32(
            PropMode::REPLACE,
            window,
            a._NET_WM_BYPASS_COMPOSITOR,
            AtomEnum::CARDINAL,
            &[2],
        )?;
        Ok(())
    }

    fn raise(&self, window: Window) -> Result<()> {
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    /// Upload every sprite as an A8 glyph (id = sprite index).
    fn upload_glyphs(&self, sheet: &SpriteSheet) -> Result<Glyphs> {
        let set = self.conn.generate_id()?;
        self.conn.render_create_glyph_set(set, self.a8_format)?;
        let max = self.conn.maximum_request_bytes().saturating_sub(64);
        let (mut ids, mut infos, mut data) = (Vec::new(), Vec::new(), Vec::new());
        for (id, sprite) in sheet.iter() {
            let size = sprite.size as usize;
            // A8 glyph rows are padded to 4 bytes.
            let stride = size.next_multiple_of(4);
            if !ids.is_empty() && data.len() + stride * size + 16 * (ids.len() + 1) > max {
                self.conn.render_add_glyphs(set, &ids, &infos, &data)?;
                (ids, infos, data) = (Vec::new(), Vec::new(), Vec::new());
            }
            ids.push(id);
            infos.push(Glyphinfo {
                width: sprite.size as u16,
                height: sprite.size as u16,
                x: 0,
                y: 0,
                x_off: 0,
                y_off: 0,
            });
            for row in sprite.alpha().chunks(size) {
                data.extend_from_slice(row);
                data.resize(data.len() + stride - size, 0);
            }
        }
        if !ids.is_empty() {
            self.conn.render_add_glyphs(set, &ids, &infos, &data)?;
        }
        let mut colors = [0; 3];
        for layer in Layer::ALL {
            let pic = self.conn.generate_id()?;
            let [red, green, blue] = layer_rgb(layer).map(|c| u16::from(c) * 257);
            let color = render::Color {
                red,
                green,
                blue,
                alpha: 0xffff,
            };
            self.conn.render_create_solid_fill(pic, color)?;
            colors[layer.index()] = pic;
        }
        log::debug!("uploaded {} sprites as XRender glyphs", sheet.len());
        Ok(Glyphs { set, colors })
    }

    fn destroy_surfaces(&mut self) -> Result<()> {
        for s in self.surfaces.drain(..) {
            self.conn.render_free_picture(s.picture)?;
            self.conn.destroy_window(s.window)?;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn is_ours(&self, window: Window) -> bool {
        self.surfaces.iter().any(|s| s.window == window)
    }

    /// Returns true if outputs may have changed.
    fn handle(&mut self, event: Event, raise: &mut bool) -> bool {
        match event {
            // Something was stacked directly above one of our windows,
            // or a new window appeared (new windows map on top).
            Event::ConfigureNotify(e) if !self.is_ours(e.window) => {
                *raise |= self.is_ours(e.above_sibling);
            }
            Event::MapNotify(e) if !self.is_ours(e.window) => *raise = true,
            Event::CirculateNotify(e) if !self.is_ours(e.window) => *raise = true,
            Event::Expose(e) => {
                if let Some(s) = self.surfaces.iter_mut().find(|s| s.window == e.window) {
                    s.full_pending = true;
                }
            }
            Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => return true,
            Event::Error(e) => log::warn!("X11 error: {e:?}"),
            _ => {}
        }
        false
    }

    fn rebuild_if_outputs_changed(&mut self) -> Result<bool> {
        let outputs = self.query_outputs(&self.screen())?;
        if outputs == self.outputs {
            return Ok(false);
        }
        log::info!("monitors changed: {} output(s)", outputs.len());
        self.destroy_surfaces()?;
        self.outputs = outputs;
        self.create_surfaces()?;
        Ok(true)
    }

    fn wait_readable(&self, timeout: Duration) -> Result<()> {
        let ts = Timespec::try_from(timeout).unwrap_or(Timespec {
            tv_sec: i64::MAX,
            tv_nsec: 0,
        });
        let fd = self.conn.stream().as_fd();
        let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn max_request_items(&self, item_bytes: usize) -> usize {
        (self.conn.maximum_request_bytes().saturating_sub(64) / item_bytes).max(1)
    }

    fn clear(&mut self, picture: render::Picture, rects: &[Rect]) -> Result<()> {
        self.rects.clear();
        self.rects.extend(rects.iter().map(to_x_rect));
        let transparent = render::Color {
            red: 0,
            green: 0,
            blue: 0,
            alpha: 0,
        };
        for chunk in self.rects.chunks(self.max_request_items(8)) {
            self.conn
                .render_fill_rectangles(PictOp::SRC, picture, transparent, chunk)?;
        }
        Ok(())
    }

    /// One CompositeGlyphs32 per run of same-layer draws (scenes are sorted
    /// back to front, so that is one request per layer).
    fn draw_sprites(
        &mut self,
        picture: render::Picture,
        draws: &[SpriteDraw],
        sheet: &SpriteSheet,
    ) -> Result<()> {
        let Some(glyphs) = &self.glyphs else {
            return Ok(());
        };
        let max_elts = self.max_request_items(GLYPH_ELT_BYTES);
        let mut start = 0;
        while start < draws.len() {
            let layer = sheet.get(draws[start].sprite).layer;
            let end = draws[start..]
                .iter()
                .position(|d| sheet.get(d.sprite).layer != layer)
                .map_or(draws.len(), |n| start + n)
                .min(start + max_elts);
            encode_glyph_cmds(&draws[start..end], &mut self.cmds);
            self.conn.render_composite_glyphs32(
                PictOp::OVER,
                glyphs.colors[layer.index()],
                picture,
                x11rb::NONE, // composite glyphs one by one: overlaps blend correctly
                glyphs.set,
                0,
                0,
                &self.cmds,
            )?;
            start = end;
        }
        Ok(())
    }
}

impl Backend for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn outputs(&self) -> &[OutputInfo] {
        &self.outputs
    }

    fn present(&mut self, output: usize, scene: &Scene, sheet: &SpriteSheet) -> Result<()> {
        if self.glyphs.is_none() {
            self.glyphs = Some(self.upload_glyphs(sheet)?);
        }
        let s = self
            .surfaces
            .get_mut(output)
            .context("present: no such output")?;
        let picture = s.picture;
        let full = std::mem::take(&mut s.full_pending);
        let full_rect = [Rect::new(0, 0, s.w.into(), s.h.into())];
        if !full && scene.clear.is_empty() && scene.draws.is_empty() {
            return Ok(());
        }
        self.clear(picture, if full { &full_rect } else { &scene.clear })?;
        self.draw_sprites(picture, &scene.draws, sheet)?;
        // One round trip per frame: back-pressure if the server falls behind.
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }

    fn dispatch(&mut self, timeout: Duration) -> Result<Vec<BackendEvent>> {
        let mut event = self.conn.poll_for_event()?;
        if event.is_none() && !timeout.is_zero() {
            self.wait_readable(timeout)?;
            event = self.conn.poll_for_event()?;
        }
        let (mut raise, mut outputs_maybe_changed) = (false, false);
        while let Some(e) = event {
            outputs_maybe_changed |= self.handle(e, &mut raise);
            event = self.conn.poll_for_event()?;
        }
        let mut events = Vec::new();
        if outputs_maybe_changed && self.rebuild_if_outputs_changed()? {
            events.push(BackendEvent::OutputsChanged);
        } else if raise {
            // At most once per dispatch, so two "always on top" clients can't
            // ping-pong us into a busy loop.
            for s in &self.surfaces {
                self.raise(s.window)?;
            }
            self.conn.flush()?;
        }
        Ok(events)
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        if let Some(g) = self.glyphs.take() {
            let _ = self.conn.render_free_glyph_set(g.set);
            for p in g.colors {
                let _ = self.conn.render_free_picture(p);
            }
        }
        if let Err(e) = self.destroy_surfaces() {
            log::debug!("X11 cleanup: {e:#}");
        }
    }
}

fn to_x_rect(r: &Rect) -> Rectangle {
    Rectangle {
        x: r.x as i16,
        y: r.y as i16,
        width: r.w as u16,
        height: r.h as u16,
    }
}

/// Encode draws as GLYPHELT32 items: one glyph each, positioned relative to
/// the previous one (glyphs have no advance, so the pen stays put).
fn encode_glyph_cmds(draws: &[SpriteDraw], out: &mut Vec<u8>) {
    out.clear();
    let (mut px, mut py) = (0i32, 0i32);
    for d in draws {
        let (dx, dy) = ((d.x - px) as i16, (d.y - py) as i16);
        (px, py) = (px + i32::from(dx), py + i32::from(dy));
        out.extend_from_slice(&[1, 0, 0, 0]); // glyph count, padding
        out.extend_from_slice(&dx.to_ne_bytes());
        out.extend_from_slice(&dy.to_ne_bytes());
        out.extend_from_slice(&d.sprite.to_ne_bytes());
    }
}

fn check_compositor(conn: &RustConnection, screen_num: usize, skip: bool) -> Result<()> {
    let name = format!("_NET_WM_CM_S{screen_num}");
    let atom = conn.intern_atom(false, name.as_bytes())?.reply()?.atom;
    let owner = conn.get_selection_owner(atom)?.reply()?.owner;
    if owner != x11rb::NONE {
        log::debug!("compositor found ({name} owned by window {owner:#x})");
        return Ok(());
    }
    if skip {
        log::warn!("no compositor found; continuing because of --no-compositor-check");
        return Ok(());
    }
    bail!(NO_COMPOSITOR_MSG)
}

fn find_argb_visual(screen: &Screen) -> Result<Visualid> {
    let visual = screen
        .allowed_depths
        .iter()
        .filter(|d| d.depth == 32)
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.class == VisualClass::TRUE_COLOR)
        .context("the X server has no 32-bit ARGB visual (is the Composite extension enabled?)")?;
    log::debug!("ARGB visual: {:#x}", visual.visual_id);
    Ok(visual.visual_id)
}

/// The XRender formats for `visual` (destination) and plain A8 (glyphs).
fn find_formats(conn: &RustConnection, visual: Visualid) -> Result<(Pictformat, Pictformat)> {
    let formats = conn.render_query_pict_formats()?.reply()?;
    let argb = formats
        .screens
        .iter()
        .flat_map(|s| s.depths.iter())
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual == visual)
        .map(|v| v.format)
        .context("XRender has no format for the ARGB visual")?;
    let a8 = formats
        .formats
        .iter()
        .find(|f| {
            f.type_ == PictType::DIRECT
                && f.depth == 8
                && f.direct.alpha_mask == 0xff
                && f.direct.red_mask == 0
                && f.direct.green_mask == 0
                && f.direct.blue_mask == 0
        })
        .map(|f| f.id)
        .context("XRender has no A8 format")?;
    Ok((argb, a8))
}

fn randr_usable(conn: &RustConnection) -> Result<bool> {
    if conn
        .extension_information(randr::X11_EXTENSION_NAME)?
        .is_none()
    {
        log::debug!("RandR not available; treating the screen as one monitor");
        return Ok(false);
    }
    let v = conn.randr_query_version(1, 3)?.reply()?;
    Ok((v.major_version, v.minor_version) >= (1, 3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_cmds_are_relative() {
        let draws = [
            SpriteDraw {
                sprite: 7,
                x: 10,
                y: 20,
            },
            SpriteDraw {
                sprite: 9,
                x: 5,
                y: 25,
            },
        ];
        let mut out = Vec::new();
        encode_glyph_cmds(&draws, &mut out);
        assert_eq!(out.len(), 2 * GLYPH_ELT_BYTES);
        let i16_at = |i: usize| i16::from_ne_bytes([out[i], out[i + 1]]);
        let u32_at = |i: usize| u32::from_ne_bytes([out[i], out[i + 1], out[i + 2], out[i + 3]]);
        assert_eq!((out[0], i16_at(4), i16_at(6), u32_at(8)), (1, 10, 20, 7));
        assert_eq!((out[12], i16_at(16), i16_at(18), u32_at(20)), (1, -5, 5, 9));
    }
}
