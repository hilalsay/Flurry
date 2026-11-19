# Project Plan: `flurry` — Desktop Snow for Linux

> Working name `flurry`; rename freely. This file is the build plan for Claude Code.
> Work **one phase at a time**, in order. Each phase ends with its acceptance checks passing
> and a git commit. Do not start the next phase until the current one is accepted.

Decisions and deviations from this plan are logged in [`DECISIONS.md`](DECISIONS.md).

---

## 1. What we are building

A lightweight app that makes snow fall over the Linux desktop:

- A transparent, **click-through**, always-on-top overlay on every monitor.
- Realistic snow: three depth layers (near flakes bigger, brighter, faster), sway, wind gusts.
- Snow **piles up** at the bottom of the screen, on panels/docks, and on top of windows. It melts slowly.
- **Interaction** (inspired by the MIT-licensed macOS app
  [WinterIsComing](https://github.com/yike-shi/WINTER_IS_COMING)): dragging a window knocks its
  snow off; moving the mouse through snow rolls snowballs; snowballs can be carried; three
  stacked snowballs become a snowman.
- Controlled from a **tray icon** and a **CLI** (`flurry toggle`, `flurry quit`, …).
- Low resource use at 1080p with default settings, in % of one core on a mid-range laptop:
  **flurry < 3%**, and **flurry + Xorg + compositor together < 12%** (see §5 "Frame rate and CPU").

### Non-goals (for now)
- Windows and macOS (later projects; keep the platform layer clean so they can be added).
- GPU rendering (CPU rendering first; GPU is a fallback, see §9).
- Santa, scenery, sounds.

---

## 2. Target environments

> **Main target and dev machine: Linux Mint, Cinnamon on X11.** Every phase must work there first.
> X11 gives us window positions, stacking order, cursor position and button state, so the
> interactive features are built on X11 first. Wayland and KDE come later (Phase 9).

| Environment | Overlay method | Priority |
|---|---|---|
| **Cinnamon (Mint) on X11** | Two windows: flakes below apps, piles overlay on top (§4 "Layering") | **P0** |
| Other X11 desktops with a compositor (GNOME on Xorg, KDE X11, XFCE, MATE, i3+picom) | Same as Cinnamon | P1 |
| Wayland: KDE Plasma, Sway, Hyprland, COSMIC, Wayfire, river | `wlr-layer-shell` surface on the overlay layer, empty input region | P2 — Phase 9 |
| Wayland: GNOME (Mutter) | Layer-shell is only experimental on Mutter 47+. Try it; else XWayland (see §9) | P3 — Phase 9 |
| X11 without a compositor | No transparency possible; show a clear error suggesting `picom` | P2 |

---

## 3. Tech stack

Language: **Rust (stable, 2021 edition)**.

| Purpose | Crate |
|---|---|
| X11 | `x11rb` (features: `shape`, `randr`, `render`) + `rustix` (`poll`) |
| Wayland + layer-shell | `smithay-client-toolkit` (includes `wayland-client`, layer-shell, `wl_shm`, output handling) |
| 2D drawing (CPU) | `tiny-skia` |
| Tray icon (StatusNotifierItem) | `ksni` |
| CLI | `clap` (derive) |
| Config | `serde` + `toml`, paths via `directories` |
| App errors | `anyhow` |
| Lock/sleep events | `zbus` (logind D-Bus signals) |
| Hyprland / Sway window info | `serde_json` over their UNIX sockets (no extra crates needed) |
| Randomness | `fastrand` |
| Logging | `log` + `env_logger` |
| PNG output for tests | `tiny-skia` (built-in `save_png`) |

Do not add other dependencies without a short justification in the commit message.
Add each crate in the phase that first uses it.

---

## 4. Architecture

```
src/
  main.rs            thin: logging, CLI parsing; start daemon or send IPC command
  lib.rs             module tree
  cli.rs             clap definitions
  config.rs          Config struct, load/save TOML, defaults
  geom.rs            Rect, OutputInfo, WindowRect, CursorState, Inputs (shared by sim + platform)
  sim/               PURE simulation — no platform code, fully unit-testable
    mod.rs           World: owns everything below; step(dt, &Inputs)
    flakes.rs        falling flakes, depth layers, wind field
    piles.rs         heightmaps for ground, panels, window tops; slumping; melting
    snowballs.rs     rolling, growth, collisions, falling, poof, snowman detection
  render.rs          draws a World into a tiny_skia::Pixmap (per output)
  platform/
    mod.rs           trait Backend + backend selection
    x11.rs           X11 overlay + window list + cursor polling
    wayland.rs       layer-shell overlay
    wininfo/         window-geometry providers for Wayland
      mod.rs         trait WindowInfo
      kwin.rs        KDE: KWin script + D-Bus bridge (see §6 "KDE Plasma")
      hyprland.rs    hyprctl socket (`j/clients`)
      sway.rs        sway IPC (`get_tree`)
      none.rs        returns no windows (screen-bottom piles only)
  tray.rs            ksni tray: Pause/Resume, Light/Medium/Heavy, Clear snow, Play mode, Quit
  ipc.rs             UNIX socket at $XDG_RUNTIME_DIR/flurry.sock for CLI commands
  power.rs           logind: pause on lock/sleep, rebuild buffers on resume
```

### Key rule
`sim/` must not import anything from `platform/`, `tray` or `ipc`. It may use `geom` and
`config`. The platform layer feeds it plain data each frame:

```rust
pub struct Inputs {
    pub outputs: Vec<OutputInfo>,      // position + size of each monitor, in global coords
    pub windows: Vec<WindowRect>,      // id, rect, stacking order (top-most last)
    pub obstacles: Vec<Rect>,          // panels/docks/taskbars
    pub cursor: Option<CursorState>,   // global position + left button down; None if unknown
}
```

### Backend trait (sketch)
```rust
pub trait Backend {
    fn outputs(&self) -> Vec<OutputInfo>;
    fn windows(&mut self) -> Vec<WindowRect>;     // may be empty if unsupported
    fn obstacles(&mut self) -> Vec<Rect>;
    fn cursor(&mut self) -> Option<CursorState>;  // None on Wayland (not exposed)
    fn present(&mut self, output: usize, pixmap: &Pixmap, damage: &[Rect]);
    fn set_input_passthrough(&mut self, passthrough: bool); // false = "play mode"
    fn dispatch(&mut self, timeout: Duration) -> Vec<BackendEvent>; // outputs changed, closed...
}
```

### Backend selection at startup
1. `--backend x11|wayland` on the CLI wins.
2. If `WAYLAND_DISPLAY` is set and the compositor advertises `zwlr_layer_shell_v1` → `wayland`.
3. Else if `DISPLAY` is set → `x11` (on GNOME Wayland this means XWayland; log a warning).
4. Else exit with a clear error.

### Layering (X11): two windows
Falling flakes should pass **behind** app windows and be visible only over the wallpaper and
desktop, while settled snow is drawn **on top**. On X11 the compositor does the hiding:

- **Flakes window (below apps)**, one per monitor. ARGB + XRender like the overlay, but a *managed*
  window: `_NET_WM_WINDOW_TYPE_DESKTOP` (or `_NET_WM_STATE_BELOW` + `SKIP_TASKBAR` + `SKIP_PAGER`
  + `STICKY`), stacked just above Nemo's desktop window and below all normal windows. The
  compositor hides flakes behind apps, menus, popups, tooltips and the panel: no mask, no lag
  while dragging, correct rounded corners and shadows.
- **Piles window (on top)**: the override-redirect, click-through overlay. It draws **only**
  settled snow, snowballs, the snowman, and snow knocked off windows. It is mostly empty, so its
  damage stays small.
- Piles on a window are still clipped by the windows above it in the stacking order (the §5 mask
  logic, used for piles only).
- Both windows have an empty input shape (click-through). Nemo's desktop icons must still get clicks.
- If the WM won't keep the flakes window between Nemo and app windows, fall back to `mask`
  layering (flakes on the top overlay, masked by window rects) and log a warning.
- Config `layering = "auto" | "two-window" | "mask" | "desktop" | "front"`: `auto` = two-window on
  X11, mask on Wayland; `desktop` = flakes below apps but no piles window; `front` = everything on top.

### Main loop
- Fixed simulation step at 60 Hz (accumulator pattern). Render at the configured FPS (default 60, battery mode 30).
- Each frame: poll backend → build `Inputs` → `world.step()` → render only changed regions → `present`.
- Sleep with `dispatch(timeout)` so the process is idle between frames.

### Pixel format (easy to get wrong)
- `tiny-skia` pixmaps are **premultiplied RGBA**.
- X11 32-bit ARGB visuals and Wayland `wl_shm` `ARGB8888` expect **premultiplied BGRA in memory** (little-endian).
- Convert by swapping R and B when copying into the shared buffer. Write a unit test for this.

---

## 5. Simulation details

### Flakes
- Three layers: far (r 0.8–1.5 px, alpha ~0.45, slow), mid (r 1.5–2.5), near (r 2.5–4, alpha ~0.9, fast).
- Velocity = base fall speed × layer factor + wind × layer factor + per-flake sine sway.
- Wind = sum of two slow sine waves + occasional gusts (random start, 2–5 s ease-in/out).
- Only **near and mid** flakes collide with piles/windows; far flakes fall behind everything and recycle at the bottom.
- Respawn above the top edge; wrap horizontally across the whole virtual desktop.

### Piles (heightmaps)
- One heightmap per surface: each output's bottom edge, each obstacle top, each window top. Resolution: 1 column per 2 px.
- A flake that hits a surface adds height to its column.
- **Slumping:** each step, if neighbouring columns differ by more than a max slope, move snow downhill. Snow at a window's edge falls off as new flakes.
- **Melting:** slow constant decay; faster for very tall piles. Cap height via config.
- **Window moved:** compute window velocity from its rect last frame vs. this frame. If speed > threshold, convert its heightmap into flying particles with that velocity (faster drag → farther), then clear it.
- **Window resized/closed/covered:** resized → resample; closed → drop snow as particles; covered by a window above → hide that part (respect stacking order).
- Draw piles as one filled, slightly blurred white path per surface with a soft blue-grey shadow line underneath.

### Snowballs (X11: cursor and buttons from `QueryPointer`)
- Cursor moving sideways through a pile (within a few px of its surface) removes snow under it and spawns or grows a snowball.
- Snowballs: circles with simple physics (gravity, friction, ball-to-ball pushing, falling off edges).
- A snowball that falls a long way **poofs** into a heap on the surface it lands on.
- Left button held over a snowball → carry it; release → drop it. No play mode needed on X11.
- Rolling works on every pile: panel, window tops, screen bottom.
- Three snowballs stacked large → medium → small (within tolerance) → replace with a snowman sprite drawn in code (carrot nose, red scarf, coal eyes).

### Frame rate and CPU
- Budget is **total cost**: flurry + Xorg + Cinnamon together **< 12% of one core** at 1080p with
  defaults; flurry alone < 3%.
- **Default `fps = 30`.** 60 fps stays available in config and in the tray menu ("Smooth (60 fps)").
- With two windows, the flakes window is damaged only in small rects around moved flakes and the
  piles window only where piles or snowballs changed. Measure Cinnamon's CPU at 30 and 60 fps
  after Phase 4 and write the numbers in `DECISIONS.md`.
- If the total still exceeds 12% at 30 fps: 1) try 24 fps, 2) lower the default flake count,
  3) pause flake animation while a window covers > 90% of a monitor (nothing visible to animate),
  keeping only the piles window live.

### Config (`~/.config/flurry/config.toml`)
```toml
intensity = "medium"      # light | medium | heavy  (150 / 300 / 600 flakes per 1080p of screen)
flake_count = 300         # overrides intensity if set; per 1920×1080 of screen area
fall_speed = 1.0
wind = 1.0
max_pile_height = 40      # px
melt_rate = 1.0
fps = 30                  # 60 = "Smooth" in the tray menu
pile_on_windows = true
interaction = true
carry_modifier = "alt"    # none | alt | super  (see §8, click conflict)
layering = "auto"         # auto | two-window | mask | desktop | front (§4)
auto_pause_fullscreen = true
monitors = "all"          # or a list of output names
```
Unknown keys are rejected with an error, so typos don't fail silently.

---

## 6. Platform details

### X11
- Find a 32-bit TrueColor visual; create a colormap; create one window covering the **whole virtual screen** (or one per RandR output if simpler for damage).
- `override_redirect = true` so the window manager does not decorate or manage it; raise it after map and whenever another window is raised (listen to `ConfigureNotify` on the root window with `SubstructureNotify`).
- Click-through: `shape` extension, set the **input** shape to an empty region (`ShapeInput`, rectangles = []).
- Check for a compositor: owner of selection `_NET_WM_CM_S<screen>`. No owner → error message suggesting `picom`.
- Draw with XRender (see changelog): sprites uploaded once as an A8 glyph set. Per frame, one
  `FillRectangles(Src, transparent)` over the damaged rects, then one `CompositeGlyphs32` per depth
  layer with a solid-fill source in the layer colour.
- Windows: `_NET_CLIENT_LIST_STACKING` on the root window; for each, get geometry translated to root coords, include `_NET_FRAME_EXTENTS`; skip minimized (`_NET_WM_STATE_HIDDEN`) and windows not on the current desktop (`_NET_WM_DESKTOP` vs `_NET_CURRENT_DESKTOP`).
- Panels/docks: windows with `_NET_WM_WINDOW_TYPE_DOCK`, or use `_NET_WM_STRUT_PARTIAL`.
- Cursor: `QueryPointer` on the root window each frame → position + button mask.
- Fullscreen detection: active window (`_NET_ACTIVE_WINDOW`) has `_NET_WM_STATE_FULLSCREEN`.

### Cinnamon specifics
- **Panel**: Cinnamon draws its panels inside the shell, so they are **not** X windows (see
  DECISIONS.md). Derive them from the per-monitor work area (`_GTK_WORKAREAS_D<n>` /
  `_NET_WORKAREA`); other desktops' `_NET_WM_WINDOW_TYPE_DOCK` windows are used too. The bottom panel's top edge is the **main pile** and must always collect
  snow. Top and side panels collect only on an upward-facing visible edge. Handle auto-hide and
  size changes (re-read on `ConfigureNotify` / `PropertyNotify`).
- **Windows that collect snow:** normal windows that are **not** maximized
  (`_NET_WM_STATE_MAXIMIZED_VERT` + `_HORZ`), not fullscreen, not minimized
  (`_NET_WM_STATE_HIDDEN`), on the current workspace, and whose top edge is more than
  `max_pile_height` px below the screen top.
- Use the **frame** rect: client geometry + `_NET_FRAME_EXTENTS`, minus `_GTK_FRAME_EXTENTS` for GTK
  client-side-decorated windows, so snow sits on the visible title bar, not on the invisible shadow.
- Maximize → that window's snow falls off as particles. Restore → it collects again from zero.
  Other workspace / minimized → keep the snow and show it again when the window returns (unless it melted).
- **Desktop icons:** Nemo draws them in a `_NET_WM_WINDOW_TYPE_DESKTOP` window. The flakes window
  stays above it, so snow shows over the wallpaper and icons, and clicks still reach the icons.
- **Tray:** Cinnamon shows StatusNotifierItem icons through `xapp-sn-watcher`; `ksni` should work.
  Verify; if it doesn't show, add a fallback using libxapp's XAppStatusIcon over D-Bus.
- **Global hotkey:** `XGrabKey` on the root window, default **Super+Shift+S** = toggle. If the combo
  is taken, log it and keep the CLI working.
- **Do Not Disturb:** watch gsettings `org.cinnamon.desktop.notifications display-notifications`
  (false = DND on) → pause. Verify the key name on Mint before relying on it.
- **Autostart:** `~/.config/autostart/flurry.desktop` works on Cinnamon as-is.

### Wayland (layer-shell) — Phase 9 (deferred)
- One layer surface **per output**, layer `Overlay`, anchored to all four edges, exclusive zone `-1`, keyboard interactivity `None`.
- Click-through: `wl_surface.set_input_region` with an **empty** `wl_region`. Play mode: set a full-surface region instead.
- Buffers: double-buffered `wl_shm` pool; use `damage_buffer` with damaged rects; drive frames with `wl_surface.frame` callbacks.
- Handle output hotplug and scale (`wl_output.scale` / fractional scale if available).
- Window geometry: through `wininfo`:
  - Hyprland: `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/.socket.sock`, command `j/clients`, plus `j/monitors` for the active workspace.
  - Sway: `$SWAYSOCK`, IPC `GET_TREE`, visible windows only.
  - KDE Plasma: `kwin` provider (below).
  - Others (COSMIC, GNOME): `none` provider → piles only at screen bottom.
- Cursor: **not available** to a click-through surface on generic Wayland → `None`. On KDE, the KWin script supplies it (below). Elsewhere interaction only works in play mode (§8).

### KDE Plasma (Wayland) — extra integration — Phase 9 (deferred)
- **KWin script bridge.** At startup, write a small JavaScript KWin script to a temp file and load it over D-Bus
  (`org.kde.KWin /Scripting org.kde.kwin.Scripting.loadScript(path, "flurry")`, then `start()`).
  The script listens to `workspace.windowAdded/Removed`, each window's `frameGeometryChanged`,
  `interactiveMoveResizeStarted/Finished`, `minimizedChanged`, `workspace.windowActivated`, and
  `workspace.currentDesktopChanged`, and sends updates back with `callDBus("org.flurry.Flurry", "/", "org.flurry.Flurry", "WindowUpdate", ...)`.
  Flurry exposes that D-Bus service with `zbus`. Unload the script on exit.
- **Cursor on KDE Wayland:** the script also sends `workspace.cursorPos` at most 60×/s
  (via a QTimer in the script or `workspace.cursorPosChanged` if available — **verify on Plasma 6**).
  Button state is not exposed → rolling snowballs works without play mode; carrying still needs play mode.
- **Window PIDs** (`window.pid`) and **fullscreen state** (`window.fullScreen`) come from the same script.
- **Global shortcut:** register `flurry toggle` and `flurry play` with KGlobalAccel over D-Bus
  (`org.kde.kglobalaccel`), so they appear in System Settings → Shortcuts. Fallback: tell the user to bind the CLI command manually.
- **Do Not Disturb:** watch `org.freedesktop.Notifications` property `Inhibited` → pause when DND is on.
- **Theme:** read Plasma's colour scheme (`~/.config/kdeglobals`) to tint pile shadows to match light/dark themes.

### Tray
- `ksni` StatusNotifierItem. Works on KDE, Cinnamon, most wlroots bars (Waybar), XFCE.
- GNOME needs the AppIndicator extension; if no tray host is found, log it and rely on the CLI.

### CLI / IPC
```
flurry               # start (daemon in foreground; use & or autostart)
flurry toggle        # pause/resume
flurry clear         # remove all settled snow
flurry intensity heavy
flurry play          # toggle play mode (interaction on Wayland)
flurry reload        # re-read config
flurry quit
```
The first instance owns the socket; later invocations send the command and exit. Users can bind
`flurry toggle` to a key in their desktop settings (Wayland has no portable global hotkeys).

### Power / session
- `zbus` on the system bus: `org.freedesktop.login1.Manager.PrepareForSleep` and session `Lock`/`Unlock`.
- Pause the loop while locked/asleep; on resume, rebuild buffers and re-query outputs.

---

## 7. Phases and acceptance checks

| # | Phase | Status |
|---|---|---|
| 0 | Scaffold | ✅ |
| 1 | Simulation + headless rendering | ✅ |
| 2 | X11 overlay | ✅ (manual check by me) |
| 3 | Controls: tray, CLI/IPC, global hotkey | ✅ |
| 4 | Two-window layering + piles on panel and screen bottom | ✅ (manual check by me) |
| 5 | Piles on windows | ✅ (manual check by me) |
| 6 | Snowballs and snowman | ✅ (manual check by me) |
| 7 | Being a good citizen (auto-pause, battery, autostart) | ✅ (manual check by me) |
| 8 | Signature features (§12) | ✅ (manual check by me) |
| 9 | Wayland + KDE (layer-shell, KWin script) — deferred | |
| 10 | Packaging (.deb for Mint first, then AppImage, AUR) | ✅ (done before 9; install check by me) |

### Phase 0 — Scaffold
- `cargo new`, module skeleton as in §4, `clap` CLI, config loading with defaults, `env_logger`.
- CI script `./check.sh` running `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- ✅ `./check.sh` passes; `flurry --help` works.

### Phase 1 — Simulation + headless rendering
- Implement `sim/flakes.rs`, wind, and `render.rs`.
- Hidden CLI command `flurry render-test --frames 300 --out frames/` runs the sim with a fake 1920×1080 output and writes PNGs.
- Unit tests: flakes stay within bounds; respawn works; same seed → same output.
- ✅ PNGs show three visible depth layers and wind drift. Sim step for 300 flakes < 0.2 ms (bench test).

### Phase 2 — X11 overlay
- `platform/x11.rs`: ARGB window, override-redirect, empty input shape, XRender drawing, compositor check; `Backend` trait and selection.
- ✅ On Cinnamon: snow visible over all windows on every monitor; clicks and typing reach apps underneath; no taskbar/Alt-Tab entry.
- Automated: under `Xvfb` (no compositor) the app exits with the compositor error message; with `--no-compositor-check` it runs for 5 s without crashing (`tests/x11_xvfb.rs`; `FLURRY_TEST_XSERVER=Xephyr` uses a nested Xephyr instead).

### Phase 3 — Controls
- `tray.rs` (`ksni`; verify on Cinnamon's `xapp-sn-watcher`), `ipc.rs`, all CLI commands, config hot-reload on SIGHUP or `flurry reload`.
- Global hotkey via `XGrabKey`, default Super+Shift+S = toggle. Tray item "Smooth (60 fps)".
- ✅ Tray menu items work; `flurry toggle` from a second terminal pauses and resumes; `flurry quit` exits cleanly and removes the socket; the hotkey toggles.

### Phase 4 — Two-window layering + piles on panel and screen bottom
- Flakes window below apps, piles overlay on top (§4 "Layering"); `layering` config with the mask fallback.
- `sim/piles.rs`: heightmaps, slumping, melting. Obstacles = Cinnamon panel(s) from dock windows / struts.
- Unit tests: snow is conserved during slumping; melting decreases it; height never exceeds the max.
- Automated: under Xvfb + `openbox` with three test windows, a screenshot shows no flake pixels inside the test windows' rects.
- ✅ Flakes pass behind apps; snow settles on the panel and screen bottom, forms slopes, melts; `flurry clear` empties it; desktop icons still clickable. CPU numbers at 30/60 fps in `DECISIONS.md`.

### Phase 5 — Piles on windows
- X11 window list with stacking, frame extents and workspace filtering (§6 "Cinnamon specifics"). The KWin/Hyprland/Sway providers are in Phase 9.
- Window drag → snow flies off with the window's velocity; maximize/close → snow falls; covered parts hidden.
- ✅ Snow collects on non-maximized window tops; dragging a window throws its snow; overlapping windows look correct; nothing collects on maximized/minimized windows or other workspaces.

### Phase 6 — Snowballs and snowman
- Cursor + buttons from `QueryPointer`; rolling, growth, collisions, carry, poof, snowman.
- ✅ (manual, on Cinnamon) roll a snowball along the panel; roll one along a window top; push one off a window edge so it falls and poofs; carry one and stack three into a snowman; with `carry_modifier = "alt"` a plain left drag does **not** pick up snowballs. If Alt conflicts with Cinnamon's Alt+drag window move, switch the default to `super` and record it in `DECISIONS.md`.
- Play mode for Wayland: Phase 9.

### Phase 7 — Being a good citizen
- Auto-pause on fullscreen, lock and sleep (logind), Do Not Disturb (Cinnamon gsettings).
- Battery mode: half the flakes (and ≤ 30 fps) on battery (`/sys/class/power_supply`).
- Autostart: `flurry --install-autostart` writes `~/.config/autostart/flurry.desktop`.
- ✅ Watching a fullscreen video pauses snow; locking the screen pauses it; resume works after suspend.

### Phase 8 — Signature features
- Build the features in §12 in the order listed there. Each one is its own commit, behind a config flag, default on unless noted.
- ✅ Each feature has a `render-test` scenario or unit test where possible, and a manual check list for me.

### Phase 9 — Wayland + KDE (deferred)
- `platform/wayland.rs` with layer-shell, per-output surfaces, empty input region, frame callbacks, hotplug; mask layering.
- `wininfo/` providers: KWin script bridge, Hyprland, Sway; KDE global shortcuts, DND, theme; play mode.
- ✅ Works on KDE Plasma Wayland and Sway/Hyprland: visible, click-through, survives monitor hotplug. Automated: headless sway for 5 s with no errors.

### Phase 10 — Packaging
- `.deb` for Mint first; then `cargo install`, `.desktop` file and icon, AppImage build script, AUR `PKGBUILD`; README with GIF.
- ✅ Fresh install on Mint from the README instructions alone.

## 8. Known problems and decisions

- **Clicks still reach the app underneath.** The overlay is click-through, so holding the left button to carry a snowball also drag-selects on the desktop or in the window below. Default `carry_modifier = "alt"`; let users set `none`. Cinnamon uses Alt+drag to move windows; if that conflicts, the default becomes `super`.
- **No cursor on Wayland (Phase 9).** Click-through layer surfaces receive no pointer events and Wayland has no global cursor query. Interaction there is play mode only.
- **No window positions on most Wayland desktops.** Only Hyprland and Sway expose them via IPC (and KDE via the KWin script). Elsewhere, piles only form at the screen bottom. Document this clearly.
- **Full-screen CPU redraws are expensive at 4K.** Always track damage rects (flake bounding boxes, changed pile spans) and present only those.

---

## 9. Fallback plans

| If this fails | Do this instead |
|---|---|
| GNOME Wayland has no layer-shell | Run on XWayland (`--backend x11`). Snow may not stay above native Wayland windows; document it. Later stretch: a small GNOME Shell extension that draws the same sim. |
| X11 window manager ignores override-redirect stacking | Re-raise on every root `ConfigureNotify`. If it still flickers, offer `--below` mode (snow behind windows, xsnow's old approach). |
| Input shape not honoured (rare WM/compositor combos) | Fall back to `--below` mode so the desktop stays usable. |
| CPU above budget even with damage tracking | 1) lower FPS, 2) fewer flakes, 3) draw flakes as pre-rendered sprites instead of anti-aliased circles, 4) last resort: GPU path with `wgpu` behind a cargo feature. |
| XRender unavailable (very old/odd X servers) | Exit with a clear message. A plain `PutImage` path could be re-added if anyone needs it. |
| Compositor cost too high at 60 fps (it is roughly fixed per frame, see changelog) | Lower `fps` (30 halves it) and pause when fullscreen/idle (Phase 8). |
| No tray host (stock GNOME) | Everything also works via CLI; print a hint to install the AppIndicator extension. |
| Hyprland/Sway IPC format changes | Provider returns no windows and logs a warning; snow still works. |
| Fractional scaling looks blurry on Wayland | Render at buffer scale from `wp_fractional_scale_v1` when present; else integer scale. |

---

## 10. Working rules for Claude Code

1. Do phases in order. At the end of each phase: run `./check.sh`, fix everything, then commit with message `phase N: <summary>`.
2. Keep `sim/` platform-free and covered by unit tests. Prefer testing behaviour through `render-test` PNGs over manual checks.
3. Any check that needs a real desktop: implement it, list exactly what to try in the phase's commit/summary, and **keep going**. Don't stop for reversible decisions; log them in `DECISIONS.md`.
4. Keep functions small; no `unwrap()` outside tests and `main` setup — use `anyhow` for app errors.
5. Log useful details at `debug` level (backend chosen, outputs, compositor found, provider used).
6. When something in this plan turns out to be wrong for a real system, update this file in the same commit and explain why in `DECISIONS.md`.

---

## 11. KDE quick reference for testing

- Check layer-shell: `wayland-info | grep zwlr_layer_shell` (package `wayland-utils`).
- List loaded KWin scripts: `qdbus org.kde.KWin /Scripting` ; KWin script logs: `journalctl --user -f | grep -i kwin`.
- Interactive KWin console for trying script snippets: `plasma-interactiveconsole --kwin`.
- Nested test session without touching your desktop: `kwin_wayland --windowed --xwayland -- konsole`
  then run flurry inside that Konsole.

---

## 12. Signature features (things other snow apps don't do)

Researched against DesktopSnowOK, Let It Rain, xsnow, GNOME snow extensions, hyprsnow, Snowhush,
and the macOS apps (Let It Snow, iSnow, Festivitas, WinterIsComing). None of these do the following.
Ordered by fun-per-effort. ★ = needs the KWin bridge or X11 window info.

| # | Feature | How it works | Effort |
|---|---|---|---|
| 1 | **Hot apps melt their snow** ★ | Map each window to its PID; read CPU usage from `/proc/<pid>/stat` once per second. Windows using lots of CPU melt their snow faster and show a faint heat shimmer / drips. Your compiler window is always bare. | S |
| 2 | **Icicles** ★ | Icicles slowly grow down from the bottom edges of windows that have had snow on them for a while (not from the panel: removed at the user's request). They drip occasionally. Moving the window fast snaps them off and they fall and shatter. | M |
| 3 | **Typing shakes snow loose** ★ | Keyboard activity in the active window (on X11 via XInput2 raw key events; on KDE use the active window's title/geometry changes as a proxy, or skip on Wayland) makes small clumps fall off its top edge. Keep it subtle. | S (X11) |
| 4 | **Frost creeps in when idle** | After N minutes with no cursor movement, frost crystals grow from the screen corners (procedural branching drawn into a cached layer). Moving the mouse wipes a clear path through it, like a fogged window; it re-grows slowly. Uses KDE cursor from the KWin script; on X11 `QueryPointer`. | M |
| 5 | **Real weather mode** (opt-in) | Fetch current weather for a configured city from Open-Meteo (free, no API key) every 15 min. Snow intensity follows real snowfall, wind follows real wind speed and direction; rain or clear sky → gentle mode or pause. Users of existing apps have asked for this; none ship it. | S |
| 6 | **Snow globe shake** | Shaking the cursor back and forth fast (detected from cursor velocity reversals) lifts settled snow into a swirling flurry around the cursor that slowly settles again. | S |
| 7 | **Music makes wind** (opt-in) | Read the default PipeWire/PulseAudio monitor's volume level (`pw-cat --record` from the monitor source or `libpulse-simple`). Bass hits become wind gusts. Needs one extra crate; justify it in the commit. | M |
| 8 | **Smart privacy pause** ★ | Pause automatically when Do Not Disturb is on (KDE), when any window is fullscreen, and when the screen is being shared (detect a PipeWire `Video/Source` stream from KWin via `pw-dump` — **verify** this works on Plasma 6 before building). macOS apps only offer a manual pause. | M |
| 9 | **Write in the snow** | With the carry modifier held, dragging through a ground pile carves a groove, so you can write a short word in the snow on the panel. Grooves fill back in as new snow falls. | S |
| 10 | **Snow moves between monitors** | Snowballs and flying snow knocked off windows travel across monitor borders using the global coordinate space instead of stopping at each screen's edge. | S |

### Rules for these features
- Every feature has a config key, e.g. `features.hot_apps_melt = true`. Opt-in ones (5, 7) default to `false`.
- Real weather mode: the city is set in config only; never look up the user's location automatically. Cache the last result; work offline.
- No feature may push CPU over the §1 budget with default settings. Measure before and after each one.
- If a ★ feature has no window info on the current desktop, it switches off silently (log at `info`).

---

## 13. Credits

Interaction ideas (window snow, knocking snow off, snowballs, snowman) are inspired by
[WinterIsComing](https://github.com/yike-shi/WINTER_IS_COMING) by yike-shi (MIT). If any code
is adapted from it, keep its copyright notice in `THIRD_PARTY.md`.
