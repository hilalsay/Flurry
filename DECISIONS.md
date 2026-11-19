# Decisions

Deviations from `PLAN.md` and choices made while building, newest last. Each entry says what was
decided and why.

- **Phase 0:** The dev machine is **Linux Mint 22.3, Cinnamon on X11**, not KDE Plasma 6 Wayland.
  The X11 overlay therefore moves to **Phase 2** and Wayland to **Phase 3**, so each one can be
  tested for real as it is built. KDE/Wayland stays P0, but it is checked on a KDE machine or in
  a nested `kwin_wayland` session. In Phase 6, the X11 window list comes before the KWin provider.
- **Phase 0:** The shared types (`Rect`, `OutputInfo`, `WindowRect`, `CursorState`, `Inputs`)
  live in `src/geom.rs`, not `platform/mod.rs`. `sim/` must not import `platform/`, so the types
  `Inputs` uses can't live there. The crate is lib + bin (`src/lib.rs` + thin `src/main.rs`), so
  tests and benches can reach every module.
- **Phase 0:** The `Backend` trait is added in Phase 2 together with its first implementation,
  not as an unused stub.
- **Phase 1:** `flake_count` (and intensity) means flakes **per 1920×1080 of screen area**, not in
  total. Otherwise two monitors would get half the density and a 4K screen a quarter.
- **Phase 1:** Flakes are drawn from pre-rendered sprites (radius in 0.25 px steps × 4×4 subpixel
  offsets) with a hand-written premultiplied src-over blit, not with tiny-skia circles. A
  tiny-skia pipeline per tiny flake would cost more than the drawing itself. tiny-skia is still
  the pixmap type and is used for piles and other shapes. Measured (release): sim step 11 µs and
  full 1080p frame 0.77 ms for 300 flakes. The full frame is mostly clearing 8 MB, so damage
  tracking in Phase 2 is required to meet the CPU budget.
- **Phase 2:** The X11 backend draws with **XRender, not MIT-SHM `PutImage`**. Measured on Cinnamon
  (i5-10300H, Intel UHD, glamor), 1080p, 300 flakes, 60 fps, % of one core:

  | Present method | flurry | Xorg | cinnamon |
  |---|---|---|---|
  | (idle baseline, no flurry) | – | 3.0 | 3.8 |
  | SHM `PutImage`, ~277 damaged 16 px-tile rects/frame | 15.5 | 48.3 | 39.3 |
  | SHM `PutImage`, one full-frame rect | 18.8 | 18.8 | 23.6 |
  | **XRender: glyph set uploaded once, 1 `FillRectangles` + 1 `CompositeGlyphs`/layer per frame** | **2.3** | **10.6** | **22.7** |
  | (XRender at 30 fps) | 1.2 | 7.2 | 16.5 |

  Xorg pays about 30 µs per small `PutImage` (a glamor upload each). Muffin pays about 15% of a core
  just for repainting at 60 Hz, even when the damage is a single 16×16 rect, so the compositor
  cost depends on **frame rate**, not on how we present. flurry's own process now meets the
  < 3% budget. The whole-system cost is dominated by the compositor and scales with `fps`.
  MIT-SHM, `memfd` and `mmap` were removed. XRender is in every X server with Composite and
  also works well over remote X.
- **Phase 2:** `render.rs` now produces a per-output `Scene` (rects to clear + sprite draws) instead
  of drawing pixels. `Backend::present(output, &Scene, &SpriteSheet)`: X11 replays it with XRender,
  and CPU backends (Wayland, `render-test`) rasterise it with `Renderer::rasterize`.
- **Phase 2:** `rustix` (already a dependency of x11rb) is used directly for `poll()` on the X
  connection with a timeout.
- **Plan update (after Phase 2):** Cinnamon on X11 is the main target; Wayland/KDE moved to Phase 9.
  New phase order: 3 controls, 4 two-window layering + panel piles, 5 window piles, 6 snowballs,
  7 good citizen, 8 signature features, 9 Wayland/KDE, 10 packaging (.deb first). Falling flakes now
  pass behind app windows (a flakes window below apps plus a piles overlay on top). CPU budget is now
  total cost < 12% of a core; flurry alone < 3%. **Default `fps` is 30.**
- **Phase 2 commit:** committed before the manual check finished, so later phases can build on it.
  Anything the check finds gets fixed in a follow-up commit.
- **Phase 3:** IPC is one text line per connection on `$XDG_RUNTIME_DIR/flurry.sock`. Binding the
  socket is also the single-instance check: a live socket makes a second daemon exit with
  "already running", and a stale one is removed. Added CLI `status` and `fps <n>` alongside the
  planned commands.
- **Phase 3:** **Pause hides the snow** (clears every window and stops the simulation) rather than
  freezing it, so the toggle works as an on/off switch.
- **Phase 3:** Tray and CLI changes (intensity, fps) apply **in memory only** and are not written to
  `config.toml`, so the user's file and its comments are never rewritten. Defaults come from the config.
- **Phase 3:** `ksni` uses its `blocking` API on the `async-io` runtime (`default-features = false`)
  instead of the default tokio, which keeps the dependency tree smaller. Verified on Cinnamon: the icon
  registers with `xapp-sn-watcher`, so no libxapp fallback is needed.
- **Phase 3:** New dependency **`signal-hook`**: std has no signal handling. Ctrl+C and SIGTERM quit
  cleanly (removing the socket) and SIGHUP reloads. Commands from the socket, tray and signals arrive on
  a channel that the main loop checks at least every 100 ms (the X poll timeout is capped).
- **Phase 3:** The hotkey is grabbed four times (plain, + Caps Lock, + Num Lock, + both) so it works
  whatever the lock state. Key repeat is debounced (400 ms). If another program owns the combo,
  flurry logs a warning and carries on. Config key `hotkey` (`""` disables it).
- **Phase 4: two-window layering works on Cinnamon.** The flakes window is a managed
  `_NET_WM_WINDOW_TYPE_DESKTOP` window, and Muffin stacks it directly above Nemo's desktop window and
  below every app window (checked in `_NET_CLIENT_LIST_STACKING`). flurry also checks this itself
  1.5 s after start and falls back to `front` layering with a warning if the WM stacks it wrongly.
  `layering = "mask"` needs the window list, so until Phase 5 it falls back to `front` with a warning.
- **Phase 4: Cinnamon's panel is not an X window.** It is drawn inside the Cinnamon shell, so there is
  no `_NET_WM_WINDOW_TYPE_DOCK` window to find. Panels are therefore also derived from the per-monitor
  work area (`_GTK_WORKAREAS_D<desktop>`, falling back to `_NET_WORKAREA`): each strip of a monitor
  outside its work area that no dock window covers counts as a panel. Re-read on root `PropertyNotify`
  for the work area / client list / current desktop, and every 2 s. Auto-hide panels reserve no work
  area, so their snow collects on the screen bottom instead.
- **Phase 4: restack feedback loop fixed (this was most of the CPU cost).** Cinnamon's screensaver
  keeps two fullscreen override-redirect windows (`cinnamon-screensaver-window`, `backup-locker`) that
  are mapped but never drawn while unlocked, and they raise themselves constantly. flurry re-raised
  itself above them about 150 times a second, and every restack made Muffin re-sort and repaint the
  stage. Now flurry only re-raises above windows that are viewable, and stops fighting a window that
  goes above it more than 5 times in a second (logged at info). **The Phase 2 Cinnamon numbers above
  were inflated by this loop.**
- **Phase 4 CPU** (Cinnamon, i5-10300H, 1080p, defaults, % of one core, 10 s samples):

  | | flurry | Xorg | cinnamon | total added |
  |---|---|---|---|---|
  | idle baseline | – | 2.1 | 0.1 | – |
  | two-window, 30 fps (default) | 0.7 | 4.6 | 0.3 | **≈ 3.4** |
  | two-window, 60 fps | 2.4 | 9.5 | 0.7 | ≈ 10.4 |

  Both are inside the 12% total budget. 30 fps stays the default.
- **Phase 4: piles.** Columns are 2 px. A landed flake adds π r² × 1.2 px² of snow, spread in a
  triangle over ± 2.5 r. Snow slides where neighbours differ by > 1 px, and a little diffusion
  ("creep") keeps the surface rounded instead of spiky. Melting is 0.012 + 0.0035 × h px/s. With
  defaults the panel pile averages about 6 px after 1 min, 17 px after 5 min and 23 px after 10 min,
  with peaks below the 40 px cap. Piles are drawn as antialiased XRender `TriStrip`s (solid near-white
  fill plus a blue-grey line 1.5 px below), clipped to the redrawn rects. A column is redrawn only
  after its height changed by ≥ 0.25 px.
- **Phase 4: visual layering check.** Neither `org.gnome.Shell.Screenshot.ScreenshotArea` nor
  `org.Cinnamon.ScreenshotArea` returned the live screen here: they gave the same dark image even
  with flurry stopped. Reading the root window doesn't give the composited picture either. So the
  evidence on Cinnamon is the stacking order plus your manual check. `tests/x11_xvfb.rs` has the
  automated openbox test from the plan (it skips until Xvfb and openbox are installed).
- **Phase 5: window list (X11).** A full pipelined query of `_NET_CLIENT_LIST_STACKING` (type,
  state, desktop, `_NET_FRAME_EXTENTS`, `_GTK_FRAME_EXTENTS`, geometry, position, parent frame, and
  then the frame geometry) takes two round trips. It runs at most every 100 ms when something
  changed (stacking, active window, workspace, a client's state/desktop/extents via
  `PropertyNotify`, map/unmap/destroy), and every 2 s regardless. **While a window is dragged**, its
  position comes straight from the frame's `ConfigureNotify` (root `SubstructureNotify`) plus the
  offset recorded at the last query, so piles follow every motion event without extra round trips.
  Verified on Cinnamon: maximized windows are seen as non-collecting and minimized ones as hidden.
  A live test window collected snow, and a fast scripted drag threw it off as particles that landed
  on the panel.
- **Phase 5: piles on windows.** Only normal (or untyped), visible, non-maximized, non-fullscreen
  windows with ≥ `max_pile_height` px of room above collect snow. Window piles move with the window.
  Moving faster than 300 px/s throws the snow off with 90% of the window's velocity. Resizing
  stretches the snow. Snow taller than 1 px at a window edge slides off as particles. Closing or
  maximizing drops the snow as particles. Minimizing or switching workspace parks the pile, and it
  comes back minus the melting that happened meanwhile. Particles: gravity 520 px/s², drag 0.6/s,
  ~36 px² each, at most 3000; they land on any pile below or vanish off-screen.
- **Phase 5: occlusion.** Every pile is clipped by the windows stacked above it and by panels (XRender
  clip = redrawn rects minus those windows). A surface is redrawn whenever the set of windows covering
  it changes. Flying snow is drawn on top of everything. Landing is not occlusion-checked: snow can
  settle on the hidden part of a covered window top and shows up once that part is uncovered.
- **Phase 5: `layering = "mask"` implemented** (flakes on top, clipped away from app windows and
  panels). It is also now the fallback when the WM won't keep the flakes window below apps, instead
  of `front`.
- **Phase 5 CPU:** the window tracking adds nothing measurable. Back to back with the Phase 4 build
  (same desktop state, 30 fps, 8 s samples): Phase 4 flurry 1.6–1.7%, Xorg 7.2–8.6%; Phase 5 flurry
  1.7–1.8%, Xorg 7.5–7.7%; cinnamon ≤ 1% for both. The Phase 4 table's lower Xorg figure (4.6%) was
  taken under different desktop conditions. Today's total is about 9% of a core, under the 12% budget.
- **Phase 5:** the KWin, Hyprland and Sway window providers stay in Phase 9 with the rest of Wayland.
- **Phase 6: carrying vs Cinnamon's Alt+drag.** Cinnamon's `mouse-button-modifier` is `<Alt>`, so an
  Alt+click that reaches a window would start moving it. While the carry modifier is held (or a ball
  is carried), flurry sets the input shape of its top window to exactly the snowballs' rects (+3 px).
  An Alt+click on a snowball therefore lands on flurry's override-redirect window, so Muffin's
  passive Alt grab on app frames never fires and the app below sees nothing. Everywhere else clicks
  still pass through. **So `carry_modifier` stays `"alt"`;** `super` and `none` work the same way.
  Cursor position, button and modifiers come from `QueryPointer` once per frame.
- **Phase 6: snowballs.** Sweeping the cursor sideways (> 40 px/s) through ≥ 2 px of snow scoops a
  ball (≥ 10 px²) and pushes it ahead of the pointer. Rolling picks up 0.06 px of snow height per px
  rolled from under the ball (radius 2.5–42 px, at most 24 balls). Balls roll with friction, ride
  along on slowly moved windows, are thrown with a fast-dragged window, fall off edges, push each
  other sideways, and rest on top of each other (sliding off if more than 45% of the lower ball's
  radius off-centre). Falling more than 160 px poofs a ball into the pile below with a small puff of
  particles. Releasing a carried ball drops it, or throws it with the pointer's velocity.
- **Phase 6: snowman.** Three balls stacked bottom → top, each ≥ 1.1× the radius of the one above and
  all at rest for 0.4 s, become a snowman drawn in code (shaded balls, coal eyes and buttons, carrot
  nose, red scarf, stick arms). It rides along with its window. If the window is flung it breaks back
  into three balls that fly off, and if its surface disappears the balls fall.
- **Phase 6: drawing.** Snowballs and snowmen are full-colour images (tiny-skia), made once per
  quantised size (balls in ½ px steps, snowmen per whole-px radii) and uploaded once as XRender ARGB
  pictures. They are composited on the top window and only redrawn when they move or when something
  else clears their area. They are drawn in front of everything (not clipped by windows above them).
- **Phase 6:** Wayland play mode stays in Phase 9.
- **Phase 7: automatic pauses.** User pause (toggle/tray/hotkey) and automatic reasons combine, and
  the snow shows only when neither applies. `flurry status` names the reasons, e.g.
  `auto-paused(fullscreen)`. Verified live on Cinnamon:
  - **fullscreen**: the active window (`_NET_ACTIVE_WINDOW`) has `_NET_WM_STATE_FULLSCREEN`, checked
    whenever the window list changes (`auto_pause_fullscreen`);
  - **Do Not Disturb**: `org.cinnamon.desktop.notifications display-notifications` (verified on
    Mint: `false` = DND on), read at start and followed with a `gsettings monitor` child process (no
    dconf internals);
  - **lock**: logind's session `LockedHint` (session from `$XDG_SESSION_ID`, falling back to
    `GetSessionByPID`) plus its `PropertiesChanged` signal. **Cinnamon's own screensaver
    `GetActive` is not trusted:** on this machine it returned `true` (active for 46 minutes) in an
    unlocked session, while logind correctly said `LockedHint = false`. Screensaver `ActiveChanged`
    signals (blanking) still pause/resume;
  - **sleep**: logind `PrepareForSleep`. On wake, flurry re-reads monitors, panels and windows and
    redraws everything. Not tested live (it would suspend the machine).
- **Phase 7: battery mode.** Every 10 s flurry reads `/sys/class/power_supply`. On battery
  (adapter offline, or no adapter and a battery discharging) it runs half the flakes at ≤ 30 fps.
  The configured values stay what the tray shows.
- **Phase 7:** `--install-autostart` / `--remove-autostart` write/remove
  `~/.config/autostart/flurry.desktop` (Exec = the running binary, `X-GNOME-Autostart-Delay=5` so the
  panel and compositor are up first).
- **Phase 7:** New dependency **`zbus`** (planned in §3; already built by ksni, same 5.19 version
  with `async-io` + `blocking-api`, so nothing new to compile). BadWindow X errors from windows
  closing between queries are logged at debug, not warn.
- **Phase 8 / feature 1, hot apps melt** (`features.hot_apps_melt = true`). Window PIDs come from
  `_NET_WM_PID`. Once a second, while some window collects snow, flurry reads `/proc/*/stat` and
  sums the CPU use of each window's process **and all its descendants**, so a terminal running a
  compiler counts as hot. Above 0.15 cores a window's snow melts (1 + 25 × load) times faster and
  drips (≈ 6 drips/s per core). The planned "heat shimmer" isn't drawn: a distortion effect would need
  the pixels behind the window, which a click-through overlay doesn't have. The drips carry the
  effect instead.
- **Phase 8 / feature 2, icicles** (`features.icicles = true`). After 40 s with ≥ 300 px² of snow on
  top, a window grows icicles (about one per 55 px, 6–30 px long, 0.25 px/s) along its **bottom**
  edge, as planned, and a panel grows them from its top edge. They melt back at 0.15 px/s without
  snow, drip now and then, and are clipped by windows stacked above. A fling (the pile's knock-off
  event) snaps them off; they fall with half the window's velocity and shatter into loose snow on the
  first pile or the screen bottom. Closing or maximizing drops them too. Drawn as two XRender
  triangles each (body + glint), redrawn only when an icicle changes by a whole pixel.
- **Phase 8 / feature 3, typing shakes snow loose** (`features.typing_shakes_snow = true`). XInput2
  `RawKeyPress` on the root window (all master devices). flurry **only counts** presses; it never
  reads which key was pressed. About one key in three knocks a small clump (≤ 1.5 px deep, 3
  columns) off the focused window's pile, popping up and sideways. Subtle by design. No XInput2 →
  the feature switches off with an info log.
- **Phase 8 / feature 4, frost** (`features.frost = true`, `features.frost_idle_minutes = 3`). Per
  monitor, a grid of 12 px cells. After the idle time, frost spreads from the four corners (each
  cell freezes more when its neighbours are frozen; random per-cell "affinity" makes the front
  ragged) out to 42% of the shorter side, thick near the corner and thin at the front. Moving the
  pointer wipes a 30 px-wide clear path. Wiped spots refreeze after ≥ 20 s once the pointer has
  been still for 10 s, and after 3 minutes of continuous activity the frost fades. Drawn on the top
  layer (it's on the "glass") with 64 crystal sprites in the same XRender glyph set: 8 needle
  directions pointing away from the corner × 2 branch patterns × 4 densities, with a haze that
  thickens with density, so neighbouring cells join into fern-like streaks. Growth runs at 4 Hz, so
  the 60 Hz step stays cheap (sim step 20 µs). Without a known pointer (Wayland) frost stays off.
- **Phase 8 / feature 5, real weather** (opt-in: `features.real_weather = false`,
  `features.weather_city = ""`). Geocoding and forecast come from Open-Meteo (no key) via the system
  `curl` (no HTTP crate) and `serde_json` (planned in §3). Snowfall ≥ 1 cm/h → heavy, ≥ 0.2 cm/h or a
  heavy-snow WMO code → medium, otherwise (light snow, rain, dry) → **gentle = light**; the plan's
  "or pause" option was left out to keep the config small. Wind: speed / 15 km/h (≤ 4) times the
  east-west component of the meteorological direction (wind from the west blows right), multiplied
  by the user's `wind`. Refresh every 15 min (retry 2 min). The coordinates and last result are
  cached in `~/.cache/flurry/weather.json`; at startup a cached result younger than 6 h applies at
  once, so it works offline. The city is only ever taken from the config. Checked live with a
  neutral city (Oslo) and temporary dirs: geocoding, forecast and cache all work. `flurry status`
  now shows the effective intensity/fps (weather, battery).
- **Phase 8 / feature 6, snow globe** (`features.snow_globe = true`, needs `interaction`). Four
  direction changes of the pointer at ≥ 900 px/s within 0.8 s count as a shake. 70% of the snow on
  piles within 260 px of the pointer is lifted (snow is conserved), as particles thrown upward and
  counter-clockwise around the pointer. They whirl, fall and settle again. 1.5 s cooldown.
- **Phase 8 / feature 7, music makes wind** (opt-in: `features.music_wind = false`). Plan option
  "`pw-cat --record` from the monitor source": implemented with **`parec --device=@DEFAULT_MONITOR@`**
  (PipeWire's PulseAudio layer, present on Mint), as 8 kHz mono s16. No extra crate is needed, so
  nothing new to justify. A two-stage 150 Hz low-pass (one stage let loud hi-hats through), RMS per
  50 ms, and a hit when a window is > 1.6× the ~2 s running average and above a noise floor, at most
  every 350 ms. Each hit kicks a 0.9 s gust (70 px/s × strength 0.2–2), alternating left/right so the
  snow sways with the beat. Only a level is computed; no audio is kept or sent anywhere. `parec` is
  killed on exit (checked: none left after `flurry quit`).
- **Phase 8 / feature 8, smart privacy pause** (`features.privacy_pause = true`). Adds to Phase 7's
  DND and focused-fullscreen pauses:
  - **any** visible fullscreen window pauses the snow, not just the focused one;
  - **screen sharing**, two signals: (a) a sharing-indicator window title (Chrome-family "… is
    sharing your screen / a window / this tab", Firefox "Sharing Indicator"); (b) `pw-dump` every 10 s
    showing a *running* `Stream/Input/Video` node. **Not** the plan's `Video/Source`: this machine
    always has a `Video/Source` node (the webcam device), so that check would pause forever. On X11,
    browsers and most apps capture the screen directly through X11 (no PipeWire stream), so (a) is
    the signal that works on Cinnamon. Zoom's native client and other apps with other indicator
    titles are not detected. Verified live with a fake indicator window: paused while it was open,
    resumed after. (b) is unit-tested only (no screen cast could be started here); verifying it on
    Plasma 6 is part of Phase 9.
- **Phase 8 / feature 9, write in the snow** (`features.write_in_snow = true`). A pile is a 1D
  height profile, which can't hold letter shapes, so each pile also has a groove mask of 2×2 px cells.
  Carry modifier + left drag through snow carves a 5 px-wide stroke (interpolated, so fast strokes
  have no gaps). Grooves are drawn as holes in the pile (the panel shows through), so you can write a
  short word in a deep enough pile. Each landed flake shortens the refill time of the grooves under
  it by 12 s, and every groove fills in after 120 s anyway (or once the snow under it has melted).
  While writing, the drag doesn't scoop or push snowballs. Pressing on a ball still picks it up.
  Checked with a scripted "HI" rendered to PNG.
- **Phase 8 / feature 10, snow moves between monitors** (`features.cross_monitor = true`). Flying
  snow and snowballs were already simulated in global coordinates; a two-monitor test now checks that
  snow thrown from monitor A reaches and is drawn on monitor B. With `cross_monitor = false`, flying
  snow stops at the side edge of the monitor it is on.
- **Phase 10 done before Phase 9.** Phase 9 (Wayland/KDE) needs a Wayland compositor to build and test
  against, and none is installed here (sway is available from apt but not installed). Packaging
  could be built and verified fully on this machine, so it went first. Reversible: Phase 9 is
  next.
- **Phase 10: packaging.** `scripts/build-deb.sh` builds the `.deb` with plain `dpkg-deb` (no
  cargo-deb). The binary is pure Rust (x11rb's own X11 connection, no libxcb), so it depends only
  on `libc6 (>= 2.34)` and `libgcc-s1`. Recommends `libglib2.0-bin` (gsettings → DND); Suggests
  `curl` (weather), `pulseaudio-utils` (parec → music wind), `pipewire-bin` (pw-dump). The release
  build is stripped (10 → 7.7 MB). It installs the binary, a desktop entry, an SVG icon and the docs.
  Checked: package metadata and contents, the extracted binary runs, and `cargo install --path .`
  works. **Not checked:** installing it with `apt` (needs sudo) and the AppImage/PKGBUILD builds
  (no `appimagetool`/`makepkg` here; the AppImage script doesn't download anything itself). The
  Homepage/source URL `github.com/flurry-snow/flurry` is a **placeholder** until the project has a
  real repository. `render-test` now also draws a fake app window, which gives the README GIF
  (`docs/flurry.gif`, 640×360, 384 KB).
- **Piles are drawn on the CPU and uploaded, not drawn with XRender trapezoids** (fix after user
  report: "laggs all the time, snowballs appear disappear every second"). On this Intel laptop Xorg
  runs `modesetting` + glamor, which has no GPU path for TriStrip/Triangles: every pile draw read
  the window back to the CPU, drew there and uploaded it again. Measured: piles took 20–45 ms per
  frame (flakes 0.2 ms), above the 33 ms frame budget. While that ran, the compositor could show
  the cleared-but-not-redrawn area, so the snowballs (drawn last) blinked. Now the existing
  tiny-skia renderer (`rasterize_piles`, already used by `render-test`) draws only the damaged
  rects into a per-output CPU pixmap, and each damaged rect goes to the window in one `PutImage`.
  The compositor never sees a half-drawn area. Overlapping rects are merged into their bounding box.
  `rasterize_piles` now builds one clip mask per frame (a separate one only when a window above cuts
  into the damage) instead of one full-screen mask per pile and icicle. Snowball/snowman images are
  no longer uploaded to the X server. Measured on the same desktop, 30 fps, battery: busy time per
  frame 12 ms → 0.75 ms. Added CPU: flurry 2.0%, Xorg +3.3%, Cinnamon +5.6% = ~11% total (under
  12%). A synthetic mouse sweep that rolled 7 balls, captured at 30 fps for 3 s, showed no frames
  with balls missing.
- **No icicles on the panel** (user request: the taskbar icicles aren't needed). Icicles now only
  grow from the bottom edges of snowy windows. Test: `no_icicles_on_the_panel`.
- **Snowballs are clickable without a modifier, from their upper half** (user request: "make
  iceballs clickable when hovering starts from their top", and any 3 stacked balls should make a
  snowman with a face and scarf). The upper half of every ball is always in the piles window's
  input region, and the window shows the `hand2` cursor there. A plain left drag carries the ball.
  Pushing and rolling now only happen from the ball's middle down to the snow, so reaching for a
  ball from above doesn't roll it away. With the carry modifier (Alt) held, the whole ball is
  clickable as before. Alt + drag still writes in the snow. Trade-off: a click on the top half of
  a ball no longer reaches the panel/window under it. That's OK because the ball covers that spot
  anyway.
- **Any three stacked balls make a snowman.** The size-order rule (big → medium → small) is gone.
  The three volumes are redistributed (total kept) so each ball's radius is at least 1.1× the one
  above (`snowman_volumes`). The existing snowman art already has coal eyes, a carrot nose, a red
  scarf, buttons and stick arms. Checked live with synthetic XTest input: rolled three balls of
  similar size (r ≈ 10), carried two of them with plain clicks (the script checked that the pointer
  was over flurry's window before each press), and a snowman was built.
