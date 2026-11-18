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
