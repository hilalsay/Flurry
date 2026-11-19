//! X11 backend. Per monitor there are two ARGB windows, both click-through
//! (empty input shape) and drawn server-side with XRender:
//!
//! - **flakes window**: a managed `_NET_WM_WINDOW_TYPE_DESKTOP` window. The
//!   window manager keeps it in the desktop layer, below every app window,
//!   so the compositor hides falling flakes behind apps, menus and panels.
//!   (`layering = "front"`: an override-redirect window on top instead.)
//! - **piles window**: an override-redirect window on top of everything,
//!   drawing only settled snow. It is mostly empty, so its damage is small.
//!
//! Flake sprites are uploaded once as an A8 glyph set; each frame is one
//! `FillRectangles` (clear) + one `CompositeGlyphs` per depth layer. Piles are
//! antialiased `TriStrip`s through a solid colour, clipped to their damage.
//! No pixels cross the wire (see DECISIONS.md for measurements).

use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::render::{
    self, ChangePictureAux, ConnectionExt as _, CreatePictureAux, Glyphinfo, PictOp, PictType,
    Pictformat,
};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClipOrdering, ColormapAlloc, ConfigureWindowAux,
    ConnectionExt as _, CreateWindowAux, EventMask, Gcontext, GetPropertyReply, GrabMode, Keycode,
    MapState, ModMask, PropMode, Rectangle, Screen, StackMode, VisualClass, Visualid, Window,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::{filter_outputs, Backend, BackendEvent, BackendOptions, Frame, Hotkey};
use crate::config::Layering;
use crate::geom::{subtract, AppWindow, CursorState, OutputInfo, Rect, WindowRect};
use crate::render::{copy_rgba_to_bgra, layer_rgb, SpriteDraw, SpriteSheet};
use crate::sim::flakes::Layer;

pub const NO_COMPOSITOR_MSG: &str = "no compositing manager is running on this X screen, \
so the snow overlay cannot be transparent.\n\
Start one (for example `picom &`) or turn on compositing in your desktop settings, \
or pass --no-compositor-check to run anyway.";

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        UTF8_STRING,
        WM_HINTS,
        WM_NORMAL_HINTS,
        _MOTIF_WM_HINTS,
        _NET_WM_NAME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DND,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_STATE,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_BELOW,
        _NET_WM_DESKTOP,
        _NET_WM_BYPASS_COMPOSITOR,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_WORKAREA,
        _NET_CURRENT_DESKTOP,
        _NET_ACTIVE_WINDOW,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_HIDDEN,
        _NET_FRAME_EXTENTS,
        _GTK_FRAME_EXTENTS,
        _NET_WM_PID,
    }
}

/// An app window as tracked between full queries.
#[derive(Debug, Clone, Copy)]
struct Client {
    id: Window,
    /// The window whose ConfigureNotify moves it: the WM's frame, or the
    /// client itself when unframed (e.g. GTK client-side decorations).
    frame: Window,
    /// Visible frame rect, root coordinates.
    rect: Rect,
    /// `rect` position minus the frame window's position, and the frame's
    /// size minus `rect`'s size: constant while the window is dragged.
    offset: (i32, i32),
    size_delta: (i32, i32),
    /// Mapped, not minimized, on the current workspace.
    visible: bool,
    collects: bool,
    fullscreen: bool,
    pid: Option<u32>,
    /// A screen-sharing indicator (by title).
    share_indicator: bool,
}

/// Re-query the window list at least this often, and at most this often.
const WINDOW_POLL: Duration = Duration::from_secs(2);
const WINDOW_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Ids for panels derived from the work area (Cinnamon draws its panels
/// inside the shell, so they are not X windows): base + output * 4 + edge.
const WORKAREA_PANEL_ID: u64 = 0xF1A5_0000_0000;

/// Key repeat sends many presses while the hotkey is held; act on one.
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(400);
/// Re-read panels at least this often, and at most every `DOCK_MIN_INTERVAL`.
const DOCK_POLL: Duration = Duration::from_secs(2);
const DOCK_MIN_INTERVAL: Duration = Duration::from_millis(250);
/// When to check that the WM really keeps the flakes window below apps.
const LAYERING_CHECK_AFTER: Duration = Duration::from_millis(1500);
/// A window that goes above ours more than this often per second is
/// fighting for the top (screensavers, lock screens): let it win.
const FIGHT_LIMIT: u32 = 5;
/// Bytes per CompositeGlyphs32 element holding one glyph.
const GLYPH_ELT_BYTES: usize = 12;

/// What the X11 backend actually does for `Layering`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Flakes below apps (managed desktop window) + piles on top.
    TwoWindow,
    /// Flakes below apps, no piles window.
    Desktop,
    /// Flakes and piles both in override-redirect windows on top.
    Front,
    /// Like `Front`, but flakes are clipped away from app windows.
    Mask,
}

impl Mode {
    fn from_config(l: Layering) -> Mode {
        match l {
            Layering::Auto | Layering::TwoWindow => Mode::TwoWindow,
            Layering::Desktop => Mode::Desktop,
            Layering::Front => Mode::Front,
            Layering::Mask => Mode::Mask,
        }
    }
}

/// One X window with an XRender picture.
#[derive(Debug, Clone, Copy)]
struct Win {
    window: Window,
    picture: render::Picture,
}

struct Surface {
    flakes: Win,
    piles: Option<Win>,
    /// Depth-32 GC for uploading pile pixels to `piles`.
    piles_gc: Option<Gcontext>,
    /// Current input region of the piles window (empty = click-through).
    input: Vec<Rect>,
}

/// Server-side drawing resources, created on first present.
struct Paints {
    glyphs: render::Glyphset,
    /// Solid-fill source pictures.
    layers: [render::Picture; 3],
}

/// What a batch of X events asks for.
#[derive(Default)]
struct Pending {
    /// Windows that went on top of ours (raise again if they are visible).
    raise_for: Vec<Window>,
    raise: bool,
    outputs_changed: bool,
    hotkey: bool,
    docks_changed: bool,
    /// The window list needs a full re-query / a frame moved.
    windows_dirty: bool,
    windows_moved: bool,
    redraw: Vec<usize>,
    keys: u32,
}

pub struct X11Backend {
    conn: RustConnection,
    root: Window,
    visual: Visualid,
    colormap: u32,
    /// XRender format of the ARGB visual, and the A8 format for glyphs/masks.
    argb_format: Pictformat,
    a8_format: Pictformat,
    atoms: Atoms,
    has_randr: bool,
    monitors: crate::config::Monitors,
    mode: Mode,
    outputs: Vec<OutputInfo>,
    surfaces: Vec<Surface>,
    paints: Option<Paints>,
    docks: Vec<WindowRect>,
    docks_queried: Option<Instant>,
    layering_check_at: Option<Instant>,
    /// Reused encoding buffers.
    cmds: Vec<u8>,
    rects: Vec<Rectangle>,
    /// Scratch buffer for pile uploads.
    upload: Vec<u8>,
    hotkey: Option<Keycode>,
    last_hotkey: Option<Instant>,
    /// Raises caused per window in the current second, and windows we stopped
    /// fighting with.
    raises: (Option<Instant>, HashMap<Window, u32>),
    ignored: HashSet<Window>,
    /// App windows, bottom-most first.
    clients: Vec<Client>,
    windows: Vec<AppWindow>,
    hidden: Vec<u64>,
    /// Clients we receive PropertyNotify from.
    watched: HashSet<Window>,
    windows_queried: Option<Instant>,
    /// The focused window is fullscreen.
    fullscreen: bool,
    active: Option<u64>,
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

        // Root events: windows being restacked/mapped (to keep the piles on
        // top) and the client list changing (panels).
        conn.change_window_attributes(
            screen.root,
            &ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
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

        let mode = Mode::from_config(opts.layering);
        log::debug!("X11 layering: {mode:?}");
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
            mode,
            outputs: Vec::new(),
            surfaces: Vec::new(),
            paints: None,
            docks: Vec::new(),
            docks_queried: None,
            layering_check_at: None,
            cmds: Vec::new(),
            rects: Vec::new(),
            upload: Vec::new(),
            hotkey: None,
            last_hotkey: None,
            raises: (None, HashMap::new()),
            ignored: HashSet::new(),
            clients: Vec::new(),
            windows: Vec::new(),
            hidden: Vec::new(),
            watched: HashSet::new(),
            windows_queried: None,
            fullscreen: false,
            active: None,
        };
        backend.listen_for_typing();
        if let Some(hk) = opts.hotkey {
            backend.hotkey = backend.grab_hotkey(hk)?;
        }
        backend.outputs = backend.query_outputs(&screen)?;
        backend.create_surfaces()?;
        backend.docks = backend.query_docks()?;
        backend.docks_queried = Some(Instant::now());
        backend.refresh_windows()?;
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

    // ----- monitors ---------------------------------------------------------

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

    // ----- windows ----------------------------------------------------------

    fn create_surfaces(&mut self) -> Result<()> {
        let rects: Vec<Rect> = self.outputs.iter().map(|o| o.rect).collect();
        for rect in rects {
            let managed = !self.is_overlay_mode();
            let flakes = self.create_win(rect, managed, "flurry flakes")?;
            let piles = match self.mode {
                Mode::Desktop => None,
                _ => {
                    let w = self.create_win(rect, false, "flurry piles")?;
                    // Clicks on snowballs (when the input region allows them)
                    // land here instead of in the app below.
                    self.conn.change_window_attributes(
                        w.window,
                        &ChangeWindowAttributesAux::new().event_mask(
                            EventMask::EXPOSURE
                                | EventMask::STRUCTURE_NOTIFY
                                | EventMask::BUTTON_PRESS
                                | EventMask::BUTTON_RELEASE,
                        ),
                    )?;
                    // A hand over the clickable part of a snowball.
                    if let Err(e) = self.set_hand_cursor(w.window) {
                        log::debug!("no hand cursor: {e:#}");
                    }
                    Some(w)
                }
            };
            let piles_gc = match piles {
                Some(w) => {
                    let gc = self.conn.generate_id()?;
                    self.conn.create_gc(
                        gc,
                        w.window,
                        &x11rb::protocol::xproto::CreateGCAux::new(),
                    )?;
                    Some(gc)
                }
                None => None,
            };
            self.surfaces.push(Surface {
                flakes,
                piles,
                piles_gc,
                input: Vec::new(),
            });
        }
        self.restack()?;
        self.conn.flush()?;
        if !self.is_overlay_mode() {
            self.layering_check_at = Some(Instant::now() + LAYERING_CHECK_AFTER);
        }
        Ok(())
    }

    /// An ARGB, click-through window covering `rect`. `managed`: a desktop-type
    /// window the WM keeps below apps; otherwise override-redirect.
    fn create_win(&mut self, rect: Rect, managed: bool, name: &str) -> Result<Win> {
        let (w, h) = (
            u16::try_from(rect.w).context("output too wide")?,
            u16::try_from(rect.h).context("output too tall")?,
        );
        let window = self.conn.generate_id()?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .override_redirect(u32::from(!managed))
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
        self.set_common_properties(window, name)?;
        if managed {
            self.set_desktop_properties(window, rect)?;
        }
        let picture = self.conn.generate_id()?;
        self.conn.render_create_picture(
            picture,
            window,
            self.argb_format,
            &CreatePictureAux::new(),
        )?;
        self.conn.map_window(window)?;
        Ok(Win { window, picture })
    }

    fn set_common_properties(&self, window: Window, name: &str) -> Result<()> {
        let c = &self.conn;
        let a = &self.atoms;
        let name = name.as_bytes();
        c.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            name,
        )?;
        c.change_property8(
            PropMode::REPLACE,
            window,
            a._NET_WM_NAME,
            a.UTF8_STRING,
            name,
        )?;
        c.change_property8(
            PropMode::REPLACE,
            window,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"flurry\0Flurry\0",
        )?;
        // DND windows get no shadow in picom's default config; Mutter/Muffin
        // don't shadow frameless ARGB windows anyway. Managed windows
        // override this with DESKTOP below.
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

    /// Hints for the managed flakes window: desktop layer, on every
    /// workspace, not in taskbars/pagers, never focused, no decorations,
    /// exactly this position and size.
    fn set_desktop_properties(&self, window: Window, rect: Rect) -> Result<()> {
        let c = &self.conn;
        let a = &self.atoms;
        c.change_property32(
            PropMode::REPLACE,
            window,
            a._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[a._NET_WM_WINDOW_TYPE_DESKTOP],
        )?;
        c.change_property32(
            PropMode::REPLACE,
            window,
            a._NET_WM_STATE,
            AtomEnum::ATOM,
            &[
                a._NET_WM_STATE_SKIP_TASKBAR,
                a._NET_WM_STATE_SKIP_PAGER,
                a._NET_WM_STATE_STICKY,
                a._NET_WM_STATE_BELOW,
            ],
        )?;
        c.change_property32(
            PropMode::REPLACE,
            window,
            a._NET_WM_DESKTOP,
            AtomEnum::CARDINAL,
            &[0xFFFF_FFFF],
        )?;
        // WM_HINTS: flags = InputHint, input = False.
        c.change_property32(
            PropMode::REPLACE,
            window,
            a.WM_HINTS,
            a.WM_HINTS,
            &[1, 0, 0, 0, 0, 0, 0, 0, 0],
        )?;
        // WM_NORMAL_HINTS: user position/size, min = max = size.
        let (x, y, w, h) = (rect.x as u32, rect.y as u32, rect.w, rect.h);
        let flags = 1 | 2 | 4 | 8 | 16 | 32;
        let mut hints = [0u32; 18];
        hints[..9].copy_from_slice(&[flags, x, y, w, h, w, h, w, h]);
        c.change_property32(
            PropMode::REPLACE,
            window,
            a.WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        )?;
        // Motif: no decorations.
        c.change_property32(
            PropMode::REPLACE,
            window,
            a._MOTIF_WM_HINTS,
            a._MOTIF_WM_HINTS,
            &[2, 0, 0, 0, 0],
        )?;
        Ok(())
    }

    /// Keep the top windows on top. In front mode the flakes window goes
    /// right under the piles window.
    fn restack(&self) -> Result<()> {
        for s in &self.surfaces {
            if self.is_overlay_mode() {
                self.raise(s.flakes.window)?;
            }
            if let Some(p) = s.piles {
                self.raise(p.window)?;
            }
        }
        Ok(())
    }

    fn raise(&self, window: Window) -> Result<()> {
        self.conn.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    /// Show the standard "hand2" cursor over `window`'s input region.
    fn set_hand_cursor(&self, window: Window) -> Result<()> {
        const XC_HAND2: u16 = 60;
        let font = self.conn.generate_id()?;
        self.conn.open_font(font, b"cursor")?;
        let cursor = self.conn.generate_id()?;
        self.conn.create_glyph_cursor(
            cursor,
            font,
            font,
            XC_HAND2,
            XC_HAND2 + 1,
            0,
            0,
            0,
            0xffff,
            0xffff,
            0xffff,
        )?;
        self.conn
            .change_window_attributes(window, &ChangeWindowAttributesAux::new().cursor(cursor))?;
        // The window keeps its own reference.
        self.conn.free_cursor(cursor)?;
        self.conn.close_font(font)?;
        Ok(())
    }

    fn destroy_surfaces(&mut self) -> Result<()> {
        for s in self.surfaces.drain(..) {
            if let Some(gc) = s.piles_gc {
                self.conn.free_gc(gc)?;
            }
            for w in std::iter::once(s.flakes).chain(s.piles) {
                self.conn.render_free_picture(w.picture)?;
                self.conn.destroy_window(w.window)?;
            }
        }
        self.conn.flush()?;
        Ok(())
    }

    fn surface_of(&self, window: Window) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|s| s.flakes.window == window || s.piles.is_some_and(|p| p.window == window))
    }

    fn is_ours(&self, window: Window) -> bool {
        self.surface_of(window).is_some()
    }

    fn is_overlay_mode(&self) -> bool {
        matches!(self.mode, Mode::Front | Mode::Mask)
    }

    fn is_top_window(&self, window: Window) -> bool {
        self.surfaces.iter().any(|s| {
            s.piles.is_some_and(|p| p.window == window)
                || (self.is_overlay_mode() && s.flakes.window == window)
        })
    }

    // ----- window manager queries ------------------------------------------

    fn window_list(&self, property: Atom) -> Result<Vec<Window>> {
        let reply = self
            .conn
            .get_property(false, self.root, property, AtomEnum::WINDOW, 0, 8192)?
            .reply()?;
        Ok(reply.value32().map(|v| v.collect()).unwrap_or_default())
    }

    /// `_NET_WM_WINDOW_TYPE` of each window (pipelined).
    fn window_types(&self, windows: &[Window]) -> Result<Vec<Vec<Atom>>> {
        let cookies = windows
            .iter()
            .map(|&w| {
                self.conn.get_property(
                    false,
                    w,
                    self.atoms._NET_WM_WINDOW_TYPE,
                    AtomEnum::ATOM,
                    0,
                    32,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(cookies
            .into_iter()
            .map(|c| {
                c.reply()
                    .ok()
                    .and_then(|r| r.value32().map(|v| v.collect()))
                    .unwrap_or_default()
            })
            .collect())
    }

    /// Panels in root coordinates: visible dock windows, plus the strips of
    /// each monitor outside its work area that no dock window covers.
    fn query_docks(&self) -> Result<Vec<WindowRect>> {
        let mut docks = self.dock_windows()?;
        let workareas = self.workareas()?;
        for (i, out) in self.outputs.iter().enumerate() {
            let o = out.rect;
            let Some(wa) = workareas
                .iter()
                .filter_map(|w| w.intersect(&o))
                .max_by_key(|r| u64::from(r.w) * u64::from(r.h))
            else {
                continue;
            };
            let strips = [
                Rect::new(
                    o.x,
                    wa.bottom(),
                    o.w,
                    (o.bottom() - wa.bottom()).max(0) as u32,
                ),
                Rect::new(o.x, o.y, o.w, (wa.y - o.y).max(0) as u32),
                Rect::new(o.x, o.y, (wa.x - o.x).max(0) as u32, o.h),
                Rect::new(wa.right(), o.y, (o.right() - wa.right()).max(0) as u32, o.h),
            ];
            for (edge, strip) in strips.into_iter().enumerate() {
                let covered = docks.iter().any(|d| {
                    d.rect
                        .intersect(&strip)
                        .is_some_and(|r| r.w * r.h * 2 > strip.w * strip.h)
                });
                if strip.w >= 2 && strip.h >= 2 && !covered {
                    docks.push(WindowRect {
                        id: WORKAREA_PANEL_ID + (i as u64) * 4 + edge as u64,
                        rect: strip,
                    });
                }
            }
        }
        Ok(docks)
    }

    /// Per-monitor work areas of the current desktop (`_GTK_WORKAREAS_Dn`,
    /// set by Mutter-based WMs like Muffin), else the single `_NET_WORKAREA`.
    fn workareas(&self) -> Result<Vec<Rect>> {
        let cardinals = |window, atom| -> Result<Vec<u32>> {
            let r = self
                .conn
                .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1024)?
                .reply()?;
            Ok(r.value32().map(|v| v.collect()).unwrap_or_default())
        };
        let desktop = cardinals(self.root, self.atoms._NET_CURRENT_DESKTOP)?
            .first()
            .copied()
            .unwrap_or(0);
        let name = format!("_GTK_WORKAREAS_D{desktop}");
        let atom = self.conn.intern_atom(true, name.as_bytes())?.reply()?.atom;
        let mut values = if atom == x11rb::NONE {
            Vec::new()
        } else {
            cardinals(self.root, atom)?
        };
        if values.is_empty() {
            let all = cardinals(self.root, self.atoms._NET_WORKAREA)?;
            let i = desktop as usize * 4;
            values = all.get(i..i + 4).map(<[u32]>::to_vec).unwrap_or_default();
        }
        Ok(values
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&[x, y, w, h]| Rect::new(x as i32, y as i32, w, h))
            .collect())
    }

    /// Visible windows of type `_NET_WM_WINDOW_TYPE_DOCK`.
    fn dock_windows(&self) -> Result<Vec<WindowRect>> {
        let clients = self.window_list(self.atoms._NET_CLIENT_LIST)?;
        let types = self.window_types(&clients)?;
        let docks: Vec<Window> = clients
            .iter()
            .zip(&types)
            .filter(|(_, t)| t.contains(&self.atoms._NET_WM_WINDOW_TYPE_DOCK))
            .map(|(&w, _)| w)
            .collect();
        let mut cookies = Vec::new();
        for &w in &docks {
            cookies.push((
                w,
                self.conn.get_window_attributes(w)?,
                self.conn.get_geometry(w)?,
                self.conn.translate_coordinates(w, self.root, 0, 0)?,
            ));
        }
        let mut out = Vec::new();
        for (w, attrs, geom, pos) in cookies {
            let (Ok(attrs), Ok(geom), Ok(pos)) = (attrs.reply(), geom.reply(), pos.reply()) else {
                continue; // the window went away meanwhile
            };
            if attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            let rect = Rect::new(
                pos.dst_x.into(),
                pos.dst_y.into(),
                geom.width.into(),
                geom.height.into(),
            );
            if rect.w >= 2
                && rect.h >= 2
                && self
                    .outputs
                    .iter()
                    .any(|o| o.rect.intersect(&rect).is_some())
            {
                out.push(WindowRect { id: w.into(), rect });
            }
        }
        Ok(out)
    }

    fn refresh_docks(&mut self) -> Result<bool> {
        self.docks_queried = Some(Instant::now());
        let docks = self.query_docks()?;
        if docks == self.docks {
            return Ok(false);
        }
        for d in &docks {
            log::debug!("panel {:#x}: {:?}", d.id, d.rect);
        }
        self.docks = docks;
        Ok(true)
    }

    /// The WM should stack our desktop-type flakes windows below every app
    /// window. If it doesn't, fall back to front layering.
    fn check_layering(&mut self) -> Result<bool> {
        let stacking = self.window_list(self.atoms._NET_CLIENT_LIST_STACKING)?;
        let ours: Vec<usize> = stacking
            .iter()
            .enumerate()
            .filter(|(_, w)| self.surfaces.iter().any(|s| s.flakes.window == **w))
            .map(|(i, _)| i)
            .collect();
        let Some(&highest) = ours.iter().max() else {
            log::debug!("flakes window not in _NET_CLIENT_LIST_STACKING (no EWMH WM?)");
            return Ok(false);
        };
        let below = &stacking[..highest];
        let types = self.window_types(below)?;
        let desktop = self.atoms._NET_WM_WINDOW_TYPE_DESKTOP;
        let app_below = below
            .iter()
            .zip(&types)
            .any(|(w, t)| !self.is_ours(*w) && !t.contains(&desktop));
        if !app_below {
            log::debug!("layering ok: flakes window is below all app windows");
            return Ok(false);
        }
        log::warn!(
            "the window manager doesn't keep the flakes window below apps; \
             falling back to layering = \"mask\" (flakes on top, cut out around windows)"
        );
        self.destroy_surfaces()?;
        self.mode = Mode::Mask;
        self.create_surfaces()?;
        Ok(true)
    }

    /// Full query of all app windows (pipelined: two round trips in total).
    fn query_clients(&mut self) -> Result<Vec<Client>> {
        let a = self.atoms;
        let current = self
            .cardinals(self.root, a._NET_CURRENT_DESKTOP)?
            .first()
            .copied()
            .unwrap_or(0);
        let stacking: Vec<Window> = self
            .window_list(a._NET_CLIENT_LIST_STACKING)?
            .into_iter()
            .filter(|w| !self.is_ours(*w))
            .collect();
        let types = self.window_types(&stacking)?;
        let c = &self.conn;
        let mut pending = Vec::new();
        for &w in &stacking {
            let prop = |atom: Atom, ty: AtomEnum| c.get_property(false, w, atom, ty, 0, 64);
            pending.push((
                prop(a._NET_WM_STATE, AtomEnum::ATOM)?,
                prop(a._NET_WM_DESKTOP, AtomEnum::CARDINAL)?,
                prop(a._NET_FRAME_EXTENTS, AtomEnum::CARDINAL)?,
                prop(a._GTK_FRAME_EXTENTS, AtomEnum::CARDINAL)?,
                prop(a._NET_WM_PID, AtomEnum::CARDINAL)?,
                c.get_property(false, w, a._NET_WM_NAME, a.UTF8_STRING, 0, 256)?,
                c.get_window_attributes(w)?,
                c.get_geometry(w)?,
                c.translate_coordinates(w, self.root, 0, 0)?,
                c.query_tree(w)?,
            ));
        }
        let values = |r: Result<GetPropertyReply, _>| -> Vec<u32> {
            r.ok()
                .and_then(|r| r.value32().map(|v| v.collect()))
                .unwrap_or_default()
        };
        let mut clients = Vec::new();
        for ((&w, ty), (state, desk, fext, gext, pid, title, attrs, geom, pos, tree)) in
            stacking.iter().zip(&types).zip(pending)
        {
            let (Ok(attrs), Ok(geom), Ok(pos), Ok(tree)) =
                (attrs.reply(), geom.reply(), pos.reply(), tree.reply())
            else {
                continue; // closed meanwhile
            };
            if ty.contains(&a._NET_WM_WINDOW_TYPE_DESKTOP)
                || ty.contains(&a._NET_WM_WINDOW_TYPE_DOCK)
            {
                continue;
            }
            let state = values(state.reply());
            let desktop = values(desk.reply()).first().copied().unwrap_or(current);
            let elsewhere = desktop != current && desktop != 0xFFFF_FFFF;
            let visible = attrs.map_state == MapState::VIEWABLE
                && !state.contains(&a._NET_WM_STATE_HIDDEN)
                && !elsewhere;
            let maximized = state.contains(&a._NET_WM_STATE_MAXIMIZED_VERT)
                && state.contains(&a._NET_WM_STATE_MAXIMIZED_HORZ);
            let fullscreen = state.contains(&a._NET_WM_STATE_FULLSCREEN);
            let normal = ty.is_empty() || ty.contains(&a._NET_WM_WINDOW_TYPE_NORMAL);
            let client = Rect::new(
                pos.dst_x.into(),
                pos.dst_y.into(),
                geom.width.into(),
                geom.height.into(),
            );
            let framed = tree.parent != self.root && tree.parent != x11rb::NONE;
            clients.push(Client {
                id: w,
                frame: if framed { tree.parent } else { w },
                rect: visible_rect(client, &values(fext.reply()), &values(gext.reply())),
                offset: (0, 0),
                size_delta: (0, 0),
                visible,
                collects: visible && normal && !maximized && !fullscreen,
                fullscreen: visible && fullscreen,
                pid: values(pid.reply()).first().copied().filter(|&p| p > 0),
                share_indicator: title.reply().is_ok_and(|t| {
                    crate::privacy::is_share_indicator(&String::from_utf8_lossy(&t.value))
                }),
            });
        }
        // Second pass: the geometry ConfigureNotify will report for each
        // frame, to follow windows while they are dragged.
        let cookies = clients
            .iter()
            .map(|c| self.conn.get_geometry(c.frame))
            .collect::<Result<Vec<_>, _>>()?;
        for (client, cookie) in clients.iter_mut().zip(cookies) {
            if let Ok(g) = cookie.reply() {
                client.offset = (
                    client.rect.x - i32::from(g.x),
                    client.rect.y - i32::from(g.y),
                );
                client.size_delta = (
                    i32::from(g.width) - client.rect.w as i32,
                    i32::from(g.height) - client.rect.h as i32,
                );
            }
        }
        // Get PropertyNotify (state, workspace, extents) from new clients.
        for c in &clients {
            if self.watched.insert(c.id) {
                self.conn.change_window_attributes(
                    c.id,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                )?;
            }
        }
        let alive: HashSet<Window> = clients.iter().map(|c| c.id).collect();
        self.watched.retain(|w| alive.contains(w));
        Ok(clients)
    }

    fn cardinals(&self, window: Window, atom: Atom) -> Result<Vec<u32>> {
        let r = self
            .conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1024)?
            .reply()?;
        Ok(r.value32().map(|v| v.collect()).unwrap_or_default())
    }

    /// Rebuild the public window lists from `clients`. Returns true if changed.
    fn publish_windows(&mut self) -> bool {
        let windows: Vec<AppWindow> = self
            .clients
            .iter()
            .filter(|c| c.visible)
            .map(|c| AppWindow {
                id: c.id.into(),
                rect: c.rect,
                collects: c.collects,
                pid: c.pid,
            })
            .collect();
        let hidden: Vec<u64> = self
            .clients
            .iter()
            .filter(|c| !c.visible)
            .map(|c| c.id.into())
            .collect();
        let changed = windows != self.windows || hidden != self.hidden;
        (self.windows, self.hidden) = (windows, hidden);
        changed
    }

    /// XInput2 raw key presses on the root window: we only count them (for
    /// "typing shakes snow loose"), never look at which keys they are.
    fn listen_for_typing(&self) {
        let ok = self
            .conn
            .extension_information(xinput::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
            && self
                .conn
                .xinput_xi_query_version(2, 0)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|v| v.major_version >= 2);
        if !ok {
            log::info!("no XInput2: typing won't shake snow loose");
            return;
        }
        let mask = xinput::EventMask {
            deviceid: 1, // XIAllMasterDevices
            mask: vec![xinput::XIEventMask::RAW_KEY_PRESS],
        };
        match self.conn.xinput_xi_select_events(self.root, &[mask]) {
            Ok(c) => {
                if let Err(e) = c.check() {
                    log::info!("cannot follow typing: {e}");
                }
            }
            Err(e) => log::info!("cannot follow typing: {e}"),
        }
    }

    /// Is the active window fullscreen? Returns true if that changed.
    fn update_fullscreen(&mut self) -> Result<bool> {
        let active = self
            .window_list(self.atoms._NET_ACTIVE_WINDOW)?
            .first()
            .copied()
            .unwrap_or(x11rb::NONE);
        self.active = (active != x11rb::NONE).then_some(u64::from(active));
        let fs = self.clients.iter().any(|c| c.id == active && c.fullscreen);
        let changed = fs != self.fullscreen;
        if changed {
            log::debug!("active window {active:#x} fullscreen: {fs}");
        }
        self.fullscreen = fs;
        Ok(changed)
    }

    fn refresh_windows(&mut self) -> Result<bool> {
        self.windows_queried = Some(Instant::now());
        self.clients = self.query_clients()?;
        let changed = self.update_fullscreen()? | self.publish_windows();
        if changed {
            for c in &self.clients {
                log::debug!(
                    "window {:#x}: {:?} visible={} collects={}",
                    c.id,
                    c.rect,
                    c.visible,
                    c.collects
                );
            }
        }
        Ok(changed)
    }

    /// Fast path while dragging: a frame moved or resized.
    fn frame_configured(&mut self, frame: Window, x: i16, y: i16, w: u16, h: u16) -> bool {
        let Some(c) = self.clients.iter_mut().find(|c| c.frame == frame) else {
            return false;
        };
        let rect = Rect::new(
            i32::from(x) + c.offset.0,
            i32::from(y) + c.offset.1,
            (i32::from(w) - c.size_delta.0).max(1) as u32,
            (i32::from(h) - c.size_delta.1).max(1) as u32,
        );
        let changed = rect != c.rect;
        c.rect = rect;
        changed
    }

    // ----- hotkey -----------------------------------------------------------

    /// Grab `hk` on the root window. Returns the keycode, or `None` (with a
    /// warning) if the key doesn't exist or another program already owns it.
    fn grab_hotkey(&self, hk: Hotkey) -> Result<Option<Keycode>> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()?;
        let per = usize::from(map.keysyms_per_keycode).max(1);
        // Latin-1 keysyms for lowercase letters and digits equal their ASCII code.
        let keysym = u32::from(hk.key);
        let Some(index) = map
            .keysyms
            .chunks(per)
            .position(|syms| syms.first() == Some(&keysym))
        else {
            log::warn!(
                "hotkey {hk}: no key for {:?} on this keyboard layout",
                hk.key
            );
            return Ok(None);
        };
        let code = min + index as u8;
        let mut mods = 0u16;
        for (on, m) in [
            (hk.shift, ModMask::SHIFT),
            (hk.ctrl, ModMask::CONTROL),
            (hk.alt, ModMask::M1),
            (hk.super_, ModMask::M4),
        ] {
            if on {
                mods |= u16::from(m);
            }
        }
        // Also grab with Caps Lock (Lock) and Num Lock (Mod2) on.
        let lock = u16::from(ModMask::LOCK);
        let num = u16::from(ModMask::M2);
        for extra in [0, lock, num, lock | num] {
            let grab = self.conn.grab_key(
                false,
                self.root,
                ModMask::from(mods | extra),
                code,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?;
            if let Err(e) = grab.check() {
                log::warn!(
                    "hotkey {hk} is already used by another program ({e}); \
                     bind `flurry toggle` in your keyboard settings or change `hotkey` in the config"
                );
                self.conn
                    .ungrab_key(code, self.root, ModMask::ANY)?
                    .check()
                    .ok();
                return Ok(None);
            }
        }
        log::info!("hotkey {hk} toggles the snow");
        Ok(Some(code))
    }

    // ----- events -----------------------------------------------------------

    fn handle(&mut self, event: Event, p: &mut Pending) {
        let is_dock = |w: Window| self.docks.iter().any(|d| d.id == u64::from(w));
        match event {
            Event::ConfigureNotify(e) if !self.is_ours(e.window) => {
                // Something was stacked directly above one of our top windows.
                if self.is_top_window(e.above_sibling) && !self.ignored.contains(&e.window) {
                    p.raise_for.push(e.window);
                }
                p.docks_changed |= is_dock(e.window);
                p.windows_moved |= self.frame_configured(e.window, e.x, e.y, e.width, e.height);
                // The stacking may have changed too.
                p.windows_dirty |= self.clients.iter().any(|c| c.frame == e.window);
            }
            Event::MapNotify(e) if !self.is_ours(e.window) => {
                // New windows map on top.
                if !self.ignored.contains(&e.window) {
                    p.raise_for.push(e.window);
                }
                p.docks_changed = true;
                p.windows_dirty = true;
            }
            Event::UnmapNotify(e) if !self.is_ours(e.window) => {
                p.docks_changed |= is_dock(e.window);
                p.windows_dirty = true;
            }
            Event::DestroyNotify(e) if !self.is_ours(e.window) => p.windows_dirty = true,
            Event::CirculateNotify(e) if !self.is_ours(e.window) => {
                if !self.ignored.contains(&e.window) {
                    p.raise_for.push(e.window);
                }
            }
            Event::PropertyNotify(e) if e.window == self.root => {
                let a = &self.atoms;
                // Client list, work area, or current desktop (per-desktop work areas).
                p.docks_changed |=
                    [a._NET_CLIENT_LIST, a._NET_WORKAREA, a._NET_CURRENT_DESKTOP].contains(&e.atom);
                p.windows_dirty |= [
                    a._NET_CLIENT_LIST_STACKING,
                    a._NET_CURRENT_DESKTOP,
                    a._NET_ACTIVE_WINDOW,
                ]
                .contains(&e.atom);
            }
            Event::PropertyNotify(e) => {
                let a = &self.atoms;
                p.windows_dirty |= [
                    a._NET_WM_STATE,
                    a._NET_WM_DESKTOP,
                    a._NET_FRAME_EXTENTS,
                    a._GTK_FRAME_EXTENTS,
                ]
                .contains(&e.atom);
            }
            Event::KeyPress(e) if Some(e.detail) == self.hotkey => {
                let now = Instant::now();
                if self.last_hotkey.is_none_or(|t| now - t > HOTKEY_DEBOUNCE) {
                    p.hotkey = true;
                }
                self.last_hotkey = Some(now);
            }
            Event::Expose(e) => {
                if let Some(i) = self.surface_of(e.window) {
                    if !p.redraw.contains(&i) {
                        p.redraw.push(i);
                    }
                }
            }
            Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => p.outputs_changed = true,
            Event::XinputRawKeyPress(_) => p.keys += 1,
            // Windows can vanish between our queries; that's not a problem.
            Event::Error(e) if e.error_kind == x11rb::protocol::ErrorKind::Window => {
                log::debug!("X11: window {:#x} went away", e.bad_value);
            }
            Event::Error(e) => log::warn!("X11 error: {e:?}"),
            _ => {}
        }
    }

    /// Should we go back on top of `windows`? Only for visible ones, and not
    /// for windows that keep raising themselves.
    fn should_raise(&mut self, mut windows: Vec<Window>) -> Result<bool> {
        windows.sort_unstable();
        windows.dedup();
        let cookies = windows
            .iter()
            .map(|&w| self.conn.get_window_attributes(w))
            .collect::<Result<Vec<_>, _>>()?;
        let visible: Vec<Window> = windows
            .into_iter()
            .zip(cookies)
            .filter_map(|(w, c)| {
                c.reply()
                    .ok()
                    .filter(|a| a.map_state == MapState::VIEWABLE)
                    .map(|_| w)
            })
            .collect();
        let now = Instant::now();
        let (since, counts) = &mut self.raises;
        if since.is_none_or(|t| now - t >= Duration::from_secs(1)) {
            *since = Some(now);
            counts.clear();
        }
        let mut raise = false;
        for w in visible {
            let n = counts.entry(w).or_insert(0);
            *n += 1;
            if *n > FIGHT_LIMIT {
                log::info!(
                    "window {w:#x} keeps raising itself above the snow; letting it stay on top"
                );
                self.ignored.insert(w);
            } else {
                raise = true;
            }
        }
        Ok(raise)
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

    // ----- drawing ----------------------------------------------------------

    /// Upload every sprite as an A8 glyph (id = sprite index) and create the
    /// solid colours.
    fn create_paints(&self, sheet: &SpriteSheet) -> Result<Paints> {
        let glyphs = self.conn.generate_id()?;
        self.conn.render_create_glyph_set(glyphs, self.a8_format)?;
        let max = self.conn.maximum_request_bytes().saturating_sub(64);
        let (mut ids, mut infos, mut data) = (Vec::new(), Vec::new(), Vec::new());
        for (id, sprite) in sheet.iter() {
            let size = sprite.size as usize;
            // A8 glyph rows are padded to 4 bytes.
            let stride = size.next_multiple_of(4);
            if !ids.is_empty() && data.len() + stride * size + 16 * (ids.len() + 1) > max {
                self.conn.render_add_glyphs(glyphs, &ids, &infos, &data)?;
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
            self.conn.render_add_glyphs(glyphs, &ids, &infos, &data)?;
        }
        let mut layers = [0; 3];
        for layer in Layer::ALL {
            let [r, g, b] = layer_rgb(layer);
            layers[layer.index()] = self.solid([r, g, b, 255])?;
        }
        log::debug!("uploaded {} sprites as XRender glyphs", sheet.len());
        Ok(Paints { glyphs, layers })
    }

    /// A solid-fill picture of straight (non-premultiplied) RGBA.
    fn solid(&self, [r, g, b, a]: [u8; 4]) -> Result<render::Picture> {
        let pic = self.conn.generate_id()?;
        // XRender colours are premultiplied, 16 bits per channel.
        let pm = |c: u8| (u32::from(c) * u32::from(a) * 257 / 255) as u16;
        let color = render::Color {
            red: pm(r),
            green: pm(g),
            blue: pm(b),
            alpha: u16::from(a) * 257,
        };
        self.conn.render_create_solid_fill(pic, color)?;
        Ok(pic)
    }

    fn max_request_items(&self, item_bytes: usize) -> usize {
        (self.conn.maximum_request_bytes().saturating_sub(64) / item_bytes).max(1)
    }

    fn set_rects(&mut self, rects: &[Rect]) {
        self.rects.clear();
        self.rects.extend(rects.iter().map(to_x_rect));
    }

    fn clear(&mut self, picture: render::Picture, rects: &[Rect]) -> Result<()> {
        self.set_rects(rects);
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
        let Some(paints) = &self.paints else {
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
                paints.layers[layer.index()],
                picture,
                x11rb::NONE, // composite glyphs one by one: overlaps blend correctly
                paints.glyphs,
                0,
                0,
                &self.cmds,
            )?;
            start = end;
        }
        Ok(())
    }

    /// Clear the scene's rects and redraw the piles there, clipped to them.
    /// Copy the changed parts of the CPU-drawn piles to the piles window.
    /// Each rect goes in one PutImage, so the compositor never sees a
    /// cleared-but-not-yet-redrawn area (no flicker), and nothing depends on
    /// the driver accelerating trapezoids (glamor doesn't: it reads the whole
    /// window back to the CPU, which made every frame take 20-40 ms).
    fn upload_piles(
        &mut self,
        window: Window,
        gc: Gcontext,
        pixels: &tiny_skia::Pixmap,
        damage: &[Rect],
    ) -> Result<()> {
        let (pw, ph) = (pixels.width(), pixels.height());
        let bounds = Rect::new(0, 0, pw, ph);
        let rects = coalesce(damage, &bounds);
        let max = self.conn.maximum_request_bytes().saturating_sub(64);
        let src = pixels.data();
        for r in rects {
            let row_bytes = r.w as usize * 4;
            let rows_per = (max / row_bytes).max(1);
            let mut y = 0;
            while y < r.h as usize {
                let n = rows_per.min(r.h as usize - y);
                self.upload.clear();
                self.upload.resize(row_bytes * n, 0);
                for k in 0..n {
                    let sy = r.y as usize + y + k;
                    let start = (sy * pw as usize + r.x as usize) * 4;
                    copy_rgba_to_bgra(
                        &src[start..start + row_bytes],
                        &mut self.upload[k * row_bytes..(k + 1) * row_bytes],
                    );
                }
                self.conn.put_image(
                    x11rb::protocol::xproto::ImageFormat::Z_PIXMAP,
                    window,
                    gc,
                    r.w as u16,
                    n as u16,
                    r.x as i16,
                    (r.y as usize + y) as i16,
                    0,
                    32,
                    &self.upload,
                )?;
                y += n;
            }
        }
        Ok(())
    }
}

/// The visible part of a client at `client` (root coords): add the WM's
/// decorations (`_NET_FRAME_EXTENTS`), drop GTK's invisible client-side
/// shadow margins (`_GTK_FRAME_EXTENTS`). Both are left, right, top, bottom.
fn visible_rect(client: Rect, frame: &[u32], gtk: &[u32]) -> Rect {
    let get = |v: &[u32], i: usize| v.get(i).copied().unwrap_or(0) as i32;
    let (l, r, t, b) = (
        get(frame, 0) - get(gtk, 0),
        get(frame, 1) - get(gtk, 1),
        get(frame, 2) - get(gtk, 2),
        get(frame, 3) - get(gtk, 3),
    );
    Rect::new(
        client.x - l,
        client.y - t,
        (client.w as i32 + l + r).max(1) as u32,
        (client.h as i32 + t + b).max(1) as u32,
    )
}

impl Backend for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn outputs(&self) -> &[OutputInfo] {
        &self.outputs
    }

    fn obstacles(&self) -> &[WindowRect] {
        &self.docks
    }

    fn windows(&self) -> &[AppWindow] {
        &self.windows
    }

    fn hidden_windows(&self) -> &[u64] {
        &self.hidden
    }

    fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    fn active_window(&self) -> Option<u64> {
        self.active
    }

    fn any_fullscreen(&self) -> bool {
        self.clients.iter().any(|c| c.fullscreen)
    }

    fn sharing_indicator(&self) -> bool {
        self.clients.iter().any(|c| c.share_indicator)
    }

    fn rescan(&mut self) -> Result<Vec<BackendEvent>> {
        let mut events = Vec::new();
        if self.rebuild_if_outputs_changed()? {
            events.push(BackendEvent::OutputsChanged);
        }
        if self.refresh_docks()? {
            events.push(BackendEvent::ObstaclesChanged);
        }
        if self.refresh_windows()? {
            events.push(BackendEvent::WindowsChanged);
        }
        events.extend((0..self.surfaces.len()).map(BackendEvent::Redraw));
        Ok(events)
    }

    fn cursor(&mut self) -> Option<CursorState> {
        let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        let mask = u16::from(reply.mask);
        let has = |m: x11rb::protocol::xproto::KeyButMask| mask & u16::from(m) != 0;
        use x11rb::protocol::xproto::KeyButMask;
        Some(CursorState {
            x: f32::from(reply.root_x),
            y: f32::from(reply.root_y),
            left_down: has(KeyButMask::BUTTON1),
            alt: has(KeyButMask::MOD1),
            super_: has(KeyButMask::MOD4),
        })
    }

    fn set_input_region(&mut self, output: usize, rects: &[Rect]) -> Result<()> {
        let Some(s) = self.surfaces.get_mut(output) else {
            return Ok(());
        };
        let Some(piles) = s.piles else {
            return Ok(());
        };
        if s.input == rects {
            return Ok(());
        }
        s.input = rects.to_vec();
        let x_rects: Vec<Rectangle> = rects.iter().map(to_x_rect).collect();
        self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            piles.window,
            0,
            0,
            &x_rects,
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn present(&mut self, output: usize, frame: &Frame) -> Result<()> {
        if self.paints.is_none() {
            self.paints = Some(self.create_paints(frame.sprites)?);
        }
        let s = self
            .surfaces
            .get(output)
            .context("present: no such output")?;
        let flakes = s.flakes.picture;
        let piles = s.piles.map(|p| p.window).zip(s.piles_gc);
        if !frame.flakes.clear.is_empty() || !frame.flakes.draws.is_empty() {
            self.clear(flakes, &frame.flakes.clear)?;
            let masked = self.mode == Mode::Mask;
            if masked {
                // Flakes only where no app window or panel is.
                let o = self.outputs.get(output).map(|o| o.rect).unwrap_or_default();
                let holes: Vec<Rect> = self
                    .windows
                    .iter()
                    .map(|w| w.rect)
                    .chain(self.docks.iter().map(|d| d.rect))
                    .map(|r| Rect::new(r.x - o.x, r.y - o.y, r.w, r.h))
                    .collect();
                let open = subtract(&[Rect::new(0, 0, o.w, o.h)], &holes);
                self.set_rects(&open);
                self.conn
                    .render_set_picture_clip_rectangles(flakes, 0, 0, &self.rects)?;
            }
            self.draw_sprites(flakes, &frame.flakes.draws, frame.sprites)?;
            if masked {
                self.conn.render_change_picture(
                    flakes,
                    &ChangePictureAux::new().clipmask(x11rb::NONE),
                )?;
            }
        }
        if let (Some((window, gc)), Some(pixels)) = (piles, frame.pile_pixels) {
            if !frame.piles.clear.is_empty() {
                self.upload_piles(window, gc, pixels, &frame.piles.clear)?;
            }
        }
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
        let mut p = Pending::default();
        while let Some(e) = event {
            self.handle(e, &mut p);
            event = self.conn.poll_for_event()?;
        }
        let now = Instant::now();
        let mut events: Vec<BackendEvent> =
            p.redraw.into_iter().map(BackendEvent::Redraw).collect();
        if p.hotkey {
            events.push(BackendEvent::Hotkey);
        }
        if p.keys > 0 {
            events.push(BackendEvent::Keys(p.keys));
        }
        p.raise = !p.raise_for.is_empty() && self.should_raise(std::mem::take(&mut p.raise_for))?;
        let mut rebuilt = p.outputs_changed && self.rebuild_if_outputs_changed()?;
        if self.layering_check_at.is_some_and(|t| now >= t) {
            self.layering_check_at = None;
            rebuilt |= self.check_layering()?;
        }
        if rebuilt {
            events.push(BackendEvent::OutputsChanged);
        } else if p.raise {
            // At most once per dispatch, so two "always on top" clients can't
            // ping-pong us into a busy loop.
            self.restack()?;
            self.conn.flush()?;
        }
        let since = self.windows_queried.map_or(WINDOW_POLL, |t| now - t);
        let requery =
            (p.windows_dirty || rebuilt) && since >= WINDOW_MIN_INTERVAL || since >= WINDOW_POLL;
        let windows_changed = if requery {
            self.refresh_windows()?
        } else {
            p.windows_moved && self.publish_windows()
        };
        if windows_changed {
            events.push(BackendEvent::WindowsChanged);
        }
        let since = self.docks_queried.map_or(DOCK_POLL, |t| now - t);
        let due = (p.docks_changed || rebuilt) && since >= DOCK_MIN_INTERVAL || since >= DOCK_POLL;
        if due && self.refresh_docks()? {
            events.push(BackendEvent::ObstaclesChanged);
        }
        Ok(events)
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        if let Some(p) = self.paints.take() {
            let _ = self.conn.render_free_glyph_set(p.glyphs);
            for pic in p.layers {
                let _ = self.conn.render_free_picture(pic);
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

/// Damage rects clipped to `bounds`, with overlap merged. Two or a few
/// rects become their bounding box when that isn't much bigger. Many small
/// ones (flying snow all over the screen) are merged per 128 px cell, so
/// the upload stays close to the real damage instead of one huge box.
fn coalesce(rects: &[Rect], bounds: &Rect) -> Vec<Rect> {
    const CELL: i32 = 128;
    let clipped: Vec<Rect> = rects.iter().filter_map(|r| r.intersect(bounds)).collect();
    let Some(first) = clipped.first() else {
        return clipped;
    };
    let area = |r: &Rect| u64::from(r.w) * u64::from(r.h);
    let bbox = clipped.iter().fold(*first, |a, r| a.union(r));
    let sum: u64 = clipped.iter().map(area).sum();
    if area(&bbox) <= sum + sum / 2 {
        return vec![bbox];
    }
    if clipped.len() <= 64 {
        return clipped;
    }
    let mut cells: std::collections::HashMap<(i32, i32), Vec<Rect>> =
        std::collections::HashMap::new();
    for r in &clipped {
        let key = (r.x.div_euclid(CELL), r.y.div_euclid(CELL));
        cells.entry(key).or_default().push(*r);
    }
    let mut out = Vec::new();
    for rs in cells.into_values() {
        let bbox = rs.iter().fold(rs[0], |a, r| a.union(r));
        let sum: u64 = rs.iter().map(area).sum();
        if area(&bbox) <= sum * 2 {
            out.push(bbox);
        } else {
            out.extend(rs);
        }
    }
    out.sort_by_key(|r| (r.y, r.x));
    out
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

    #[test]
    fn visible_rect_adds_frame_and_drops_gtk_shadow() {
        let client = Rect::new(100, 100, 400, 300);
        // Server-side decorations: 1 px borders, 30 px title bar.
        assert_eq!(
            visible_rect(client, &[1, 1, 30, 1], &[]),
            Rect::new(99, 70, 402, 331)
        );
        // GTK CSD: no WM frame, 20 px invisible shadow (26 at the bottom).
        assert_eq!(
            visible_rect(client, &[0, 0, 0, 0], &[20, 20, 20, 26]),
            Rect::new(120, 120, 360, 254)
        );
    }

    #[test]
    fn coalesce_merges_overlap_and_clips() {
        let b = Rect::new(0, 0, 100, 100);
        // Heavily overlapping: one bounding box.
        assert_eq!(
            coalesce(&[Rect::new(0, 0, 50, 50), Rect::new(10, 10, 50, 50)], &b),
            vec![Rect::new(0, 0, 60, 60)]
        );
        // Far apart: kept separate, and clipped to the screen.
        assert_eq!(
            coalesce(&[Rect::new(0, 0, 10, 10), Rect::new(95, 95, 10, 10)], &b),
            vec![Rect::new(0, 0, 10, 10), Rect::new(95, 95, 5, 5)]
        );
        assert!(coalesce(&[Rect::new(200, 200, 5, 5)], &b).is_empty());
        // Many small rects spread over a big screen: merged per cell, not
        // into one screen-sized box.
        let big = Rect::new(0, 0, 1920, 1080);
        let tiles: Vec<Rect> = (0..200)
            .map(|i| Rect::new((i * 97) % 1900, (i * 53) % 1060, 16, 16))
            .collect();
        let out = coalesce(&tiles, &big);
        let covered: u64 = out.iter().map(|r| u64::from(r.w) * u64::from(r.h)).sum();
        assert!(
            covered <= 200 * 16 * 16 * 2,
            "upload stays small: {covered}"
        );
        for t in &tiles {
            assert!(
                out.iter().any(|r| r.intersect(t) == Some(*t)),
                "{t:?} covered"
            );
        }
    }
}
