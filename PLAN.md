# Project Plan: `flurry` — Desktop Snow for Linux

> Working name `flurry`; rename freely. This file is the build plan for Claude Code.
> Work **one phase at a time**, in order. Each phase ends with its acceptance checks passing
> and a git commit. Do not start the next phase until the current one is accepted.

## Plan changelog
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
- Low resource use: target **< 3% CPU** on a mid-range laptop at 1080p with default settings.

### Non-goals (for now)
- Windows and macOS (later projects; keep the platform layer clean so they can be added).
- GPU rendering (CPU rendering first; GPU is a fallback, see §9).
- Santa, scenery, sounds.

---

## 2. Target environments

> **Primary dev machine: Linux Mint 22.3, Cinnamon on X11** (see changelog). Every phase must work
> there first. KDE Plasma 6 Wayland is still P0 and gets extra integration (KWin script, global
> shortcuts) that other desktops don't; it is verified on a KDE machine or a nested session.

| Environment | Overlay method | Priority |
|---|---|---|
| X11 (any desktop with a compositor: Cinnamon, GNOME on Xorg, KDE X11, XFCE, MATE, i3+picom) | ARGB override-redirect window, empty input shape | **P0** |
| Wayland: KDE Plasma, Sway, Hyprland, COSMIC, Wayfire, river | `wlr-layer-shell` surface on the overlay layer, empty input region | **P0** |
| Wayland: GNOME (Mutter) | Layer-shell is only experimental on Mutter 47+. Try it; else fall back to XWayland (see §9) | P1 |
| X11 without a compositor | No transparency possible; show a clear error suggesting `picom` | P2 |

---

## 3. Tech stack

Language: **Rust (stable, 2021 edition)**.

| Purpose | Crate |
|---|---|
| X11 | `x11rb` (features: `shape`, `shm`, `randr`) |
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

### Snowballs (needs cursor; X11 only at first)
- Cursor moving sideways through a pile (within a few px of its surface) removes snow under it and spawns or grows a snowball.
- Snowballs: circles with simple physics (gravity, friction, ball-to-ball pushing, falling off edges).
- A snowball that falls a long way **poofs** into a heap on the surface it lands on.
- Left button held over a snowball → carry it; release → drop it.
- Three snowballs stacked large → medium → small (within tolerance) → replace with a snowman sprite drawn in code (carrot nose, red scarf, coal eyes).

### Config (`~/.config/flurry/config.toml`)
```toml
intensity = "medium"      # light | medium | heavy  (150 / 300 / 600 flakes per 1080p of screen)
flake_count = 300         # overrides intensity if set; per 1920×1080 of screen area
fall_speed = 1.0
wind = 1.0
max_pile_height = 40      # px
melt_rate = 1.0
fps = 60
pile_on_windows = true
interaction = true
carry_modifier = "alt"    # none | alt | super  (see §8, click conflict)
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
- Present via MIT-SHM `PutImage` (fallback: plain `PutImage`) for damaged rects only.
- Windows: `_NET_CLIENT_LIST_STACKING` on the root window; for each, get geometry translated to root coords, include `_NET_FRAME_EXTENTS`; skip minimized (`_NET_WM_STATE_HIDDEN`) and windows not on the current desktop (`_NET_WM_DESKTOP` vs `_NET_CURRENT_DESKTOP`).
- Panels/docks: windows with `_NET_WM_WINDOW_TYPE_DOCK`, or use `_NET_WM_STRUT_PARTIAL`.
- Cursor: `QueryPointer` on the root window each frame → position + button mask.
- Fullscreen detection: active window (`_NET_ACTIVE_WINDOW`) has `_NET_WM_STATE_FULLSCREEN`.

### Wayland (layer-shell)
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

### KDE Plasma (Wayland) — extra integration
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

### Phase 0 — Scaffold
- `cargo new`, module skeleton as in §4, `clap` CLI, config loading with defaults, `env_logger`.
- CI script `./check.sh` running `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
- ✅ `./check.sh` passes; `flurry --help` works.

### Phase 1 — Simulation + headless rendering
- Implement `sim/flakes.rs`, wind, and `render.rs`.
- Add a hidden CLI command `flurry render-test --frames 300 --out frames/` that runs the sim with a fake 1920×1080 output and writes PNGs.
- Unit tests: flakes stay within bounds; respawn works; same seed → same output.
- ✅ PNGs show three visible depth layers and wind drift. Sim step for 300 flakes < 0.2 ms (add a simple bench test).

### Phase 2 — X11 overlay (dev machine first)
- `platform/x11.rs`: ARGB window, override-redirect, empty input shape, MIT-SHM present, compositor check.
- `platform/mod.rs`: the `Backend` trait and backend selection (§4).
- Test on the Cinnamon X11 session (Muffin is the compositor), or in a nested `Xephyr` + `picom` session.
- ✅ On an X11 session: snow visible over all windows on every monitor; clicks and typing reach apps underneath; no taskbar/Alt-Tab entry; CPU < 3% at 1080p.
- Automated: under `Xvfb` (no compositor) the app exits with the compositor error message; with `--no-compositor-check` it runs for 5 s without crashing.

### Phase 3 — Wayland overlay
- `platform/wayland.rs` with layer-shell, per-output surfaces, empty input region, frame callbacks, hotplug.
- Develop in a nested session (`kwin_wayland --windowed`, §11, or headless sway); final check on a real KDE Plasma Wayland machine.
- ✅ Works on KDE Plasma Wayland and on Sway/Hyprland: visible over windows, fully click-through, survives plugging/unplugging a monitor; CPU < 3% at 1080p (`top`).
- Automated: run under headless sway (`WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 sway -c /dev/null`) for 5 s with no errors.
- If the layer-shell global is missing (GNOME Wayland), fall back to the X11 backend through XWayland and log a warning.

### Phase 4 — Controls
- `tray.rs`, `ipc.rs`, all CLI commands, config hot-reload on SIGHUP or `flurry reload`.
- KDE: register global shortcuts with KGlobalAccel (default Meta+Shift+S = toggle).
- ✅ Tray menu items work; `flurry toggle` from a second terminal pauses and resumes; `flurry quit` exits cleanly and removes the socket.

### Phase 5 — Piles on screen bottom and panels
- `sim/piles.rs`: heightmaps, slumping, melting; obstacles from X11 dock windows / struts.
- Unit tests: total snow is conserved during slumping; melting decreases it; height never exceeds max.
- ✅ Snow visibly settles at the bottom and on the panel, forms natural slopes, melts over time; `flurry clear` empties it.

### Phase 6 — Piles on windows
- **X11 window list first** (dev machine), with stacking and desktop filtering; then the KWin script provider, then Hyprland and Sway providers.
- Window drag → snow flies off with the window's velocity; close → snow falls; covered parts hidden.
- ✅ Snow collects on window tops; dragging a window throws its snow; overlapping windows look correct; nothing collects on minimized windows or other workspaces.

### Phase 7 — Interaction (snowballs, snowman)
- X11 cursor polling; snowball rolling, growth, collisions, carry, poof, snowman.
- Play mode toggle for Wayland (input region on, cursor from `wl_pointer` events, Esc or `flurry play` to leave).
- ✅ Can roll, carry and stack snowballs into a snowman on X11; same in play mode on Wayland; with `carry_modifier = "alt"` a plain left drag does **not** pick up snowballs.

### Phase 8 — Being a good citizen
- Auto-pause on fullscreen (X11), lock and sleep (logind).
- Battery mode: 30 FPS and half the flakes when on battery (`/sys/class/power_supply`).
- Autostart: `flurry --install-autostart` writes `~/.config/autostart/flurry.desktop`.
- ✅ Watching a fullscreen video pauses snow; locking the screen pauses it; resume works after suspend.

### Phase 9 — Signature features
- Build the features in §12 in the order listed there. Each one is its own commit, behind a config flag, default on unless noted.
- ✅ Each feature has a `render-test` scenario or unit test where possible, and a manual check list for me.

### Phase 10 — Packaging
- `cargo install` works; `.desktop` file and icon; AppImage build script; AUR `PKGBUILD`; README with GIF.
- ✅ Fresh install on Ubuntu/Mint (X11 and Wayland) and Arch/KDE works from the README instructions alone.

---

## 8. Known problems and decisions

- **Clicks still reach the app underneath.** The overlay is click-through, so holding the left button to carry a snowball also drag-selects on the desktop or in the window below. Default `carry_modifier = "alt"`; let users set `none`.
- **No cursor on Wayland.** Click-through layer surfaces receive no pointer events and Wayland has no global cursor query. Interaction there is play mode only.
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
| MIT-SHM unavailable (remote X) | Plain `PutImage` with damage rects. |
| No tray host (stock GNOME) | Everything also works via CLI; print a hint to install the AppIndicator extension. |
| Hyprland/Sway IPC format changes | Provider returns no windows and logs a warning; snow still works. |
| Fractional scaling looks blurry on Wayland | Render at buffer scale from `wp_fractional_scale_v1` when present; else integer scale. |

---

## 10. Working rules for Claude Code

1. Do phases in order. At the end of each phase: run `./check.sh`, fix everything, then commit with message `phase N: <summary>`.
2. Keep `sim/` platform-free and covered by unit tests. Prefer testing behaviour through `render-test` PNGs over manual checks.
3. Any check that needs a real desktop (Phases 2, 3, 6, 7, 8, 9): implement it, then **stop and ask me to verify**, listing exactly what to try.
4. Keep functions small; no `unwrap()` outside tests and `main` setup — use `anyhow` for app errors.
5. Log useful details at `debug` level (backend chosen, outputs, compositor found, provider used).
6. When something in this plan turns out to be wrong for a real system, update this file in the same commit and explain why (add a line to the changelog at the top).

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
| 2 | **Icicles** ★ | Icicles slowly grow down from the bottom edges of windows that have had snow on them for a while, and from the panel's top edge. They drip occasionally. Moving the window fast snaps them off and they fall and shatter. | M |
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
