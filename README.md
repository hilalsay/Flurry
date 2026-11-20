# ❄ flurry: snow on your Linux desktop

![Snow falling behind a window, piling up on its title bar and on the panel, with icicles](docs/flurry.gif)

flurry makes it snow on your desktop. The snow doesn't just fall across the screen: it falls
**behind** your windows and **piles up on top of them**, on the taskbar and along the bottom of the
screen, the way snow would settle on real objects. You can play with it: roll snowballs, build a
snowman, knock icicles down, or start a snow storm.

It runs quietly in the background, using a few percent of one CPU core, and pauses itself when you
watch a fullscreen video, play a game or share your screen.

> **Works on:**
> - **X11:** Linux Mint (Cinnamon), KDE Plasma, GNOME on Xorg, XFCE and MATE (with a compositor).
> - **Wayland:** KDE Plasma, Sway and Hyprland, plus other compositors with layer-shell (on those,
>   snow piles only at the bottom of the screen, since they don't say where windows are).
> - **GNOME on Wayland:** flurry runs through XWayland, so snow falls and piles at the bottom, but it
>   can't stay behind native Wayland windows. For the full effect, pick "GNOME on Xorg" at login.

---

## What it does

### The snow

- **Falls in depth.** Flakes come in three layers. Far ones are small, faint and slow; near ones
  are big, bright and fast. About one in eight of the nearer flakes is a real **six-armed snow
  crystal** that slowly turns as it falls.
- **Blows in the wind.** A gentle breeze drifts it, with an occasional gust.
- **Piles up.** Snow settles on the tops of windows, on the taskbar and at the bottom of the screen.
  Piles slide down when they get too steep, slowly melt, and drip off window edges.
- **Moves with your windows.** Move a window slowly and its snow rides along. Drag it fast or shake
  it and the snow flies off.
- **Grows icicles.** Icicles slowly grow under windows that have snow on them.

### Things you can do with it

| Try this | What happens |
|---|---|
| Move the mouse **sideways through a snow pile** | You **roll a snowball**. Keep pushing it and it grows. |
| Point at the **top half of a snowball** (the cursor becomes a hand), then **click and drag** | You **carry** the ball. Let go to drop it, or flick to throw it. If it falls too far it goes *poof*. Hold **Alt** to grab a ball anywhere. |
| **Stack three snowballs** on top of each other | They become a **snowman** with a face, a scarf and stick arms. The biggest ball always ends up at the bottom. |
| Hold **Alt** and **drag through a deep pile** | You **write in the snow**. |
| **Shake the mouse** quickly from side to side | **Snow globe:** the snow near the pointer whirls up into the air. |
| **Swipe the mouse fast** through icicles | The icicles **break off** and fall. They grow back over time. |
| Leave the mouse **alone for a few minutes** | **Frost** creeps in from the screen corners. Move the mouse to wipe it away, like a fogged window. |
| Run `flurry tornado` (or *Tornado!* in the tray menu) | A **30-second snow storm**. The wind rises, snow falls much harder, and a swirling tornado crosses the screen behind your windows. It sucks up the piles it passes and throws snowballs and snowmen around. |

Some smaller touches:

- **Typing** knocks little clumps of snow off the window you are typing in.
- **Busy programs melt their snow.** A window running something heavy, like a compiler or a game,
  melts its snow faster.

### It stays out of your way

- Snowballs and snowmen sitting on a window hide behind any window that covers them, and can't be
  clicked through it. Everywhere else your clicks go to your apps as normal.
- flurry **pauses by itself** while a window is fullscreen, the screen is locked, the screen is
  being shared, *Do Not Disturb* is on, or the computer is asleep.
- **On battery** it uses half the flakes and at most 30 frames per second.

### Optional extras (off until you turn them on)

- **Real weather:** the snow follows the actual weather in a city you choose. Heavy snow there means
  heavy snow on your screen, and wind there means wind here.
- **Music wind:** the bass in the music you play makes gusts of wind.

See [Settings](#settings) to turn these on.

---

## Install

You need [Rust](https://rustup.rs) to build flurry.

### Linux Mint, Ubuntu, Debian

```bash
git clone https://github.com/hilalsay/Flurry.git
cd Flurry
./scripts/build-deb.sh                        # builds target/flurry_<version>_amd64.deb
sudo apt install ./target/flurry_*_amd64.deb
```

**Updating:** run `git pull`, build again, then install with
`sudo apt install --reinstall ./target/flurry_*_amd64.deb`. Restart flurry afterwards (`flurry quit`,
then start it again).

### Other distributions

```bash
cargo install --path .        # installs ~/.cargo/bin/flurry
```

- **Arch Linux:** use `packaging/PKGBUILD`.
- **AppImage:** run `scripts/build-appimage.sh` (needs `appimagetool`).

### Uninstall

```bash
flurry --remove-autostart     # if you turned on autostart
sudo apt remove flurry        # or: cargo uninstall flurry
```

Your settings stay in `~/.config/flurry/`. Delete that folder too if you want them gone.

---

## Start and control it

Start **Flurry** from the applications menu, or from a terminal:

```bash
flurry &
```

A ❄ appears in the system tray. Click it to pause or resume. Right-click it for the menu, which has
pause, how much snow, smooth mode, tornado, clear snow and quit.

**Start it automatically at login:**

```bash
flurry --install-autostart        # undo with: flurry --remove-autostart
```

**Keyboard shortcut:** **Super + Shift + S** pauses and resumes the snow. (Super is the Windows key.)

**Terminal commands** (they control the flurry that is already running):

| Command | What it does |
|---|---|
| `flurry toggle` | Pause or resume. |
| `flurry intensity light` / `medium` / `heavy` | Less or more snow. |
| `flurry fps 60` | Smoother motion. 30 is the default and uses about half the CPU. |
| `flurry clear` | Sweep away all the settled snow. |
| `flurry tornado` | Start a 30-second snow storm with a tornado. |
| `flurry status` | Show whether it's running and its current settings. |
| `flurry reload` | Re-read the settings file after you edit it. |
| `flurry quit` | Stop flurry. |

---

## Settings

Settings live in `~/.config/flurry/config.toml`. The file is optional: create it only if you want
to change something, and write only the lines you want to change. After editing, run
`flurry reload`. Changes to `monitors`, `layering` and `hotkey` need a restart instead.

Here is every setting with its default value:

```toml
# --- How much snow, and how it moves ---
intensity = "medium"      # light | medium | heavy
# flake_count = 300       # exact number of flakes per 1920×1080 of screen (overrides intensity)
fall_speed = 1.0          # 2.0 = twice as fast
wind = 1.0                # 0 = no wind
max_pile_height = 40      # how deep piles can get, in pixels
melt_rate = 1.0           # 0 = never melts, 2.0 = melts twice as fast
fps = 30                  # 60 is smoother but uses more CPU

# --- Behaviour ---
pile_on_windows = true    # snow settles on window tops (false: only taskbar and screen bottom)
interaction = true        # snowballs, snowmen and other mouse play
carry_modifier = "alt"    # key for grabbing a whole ball and writing in snow: none | alt | super
auto_pause_fullscreen = true
monitors = "all"          # or a list, e.g. ["DP-1", "eDP-1"]
layering = "auto"         # leave on auto unless snow shows up in front of windows (see below)
hotkey = "super+shift+s"  # "" turns the shortcut off

[features]
hot_apps_melt = true      # busy programs melt their window's snow
icicles = true
typing_shakes_snow = true
frost = true
frost_idle_minutes = 3.0  # how long the mouse must rest before frost appears
snow_globe = true         # shake the mouse to whirl up snow
write_in_snow = true
privacy_pause = true      # pause while the screen is shared
cross_monitor = true      # flakes drift from one monitor onto the next
real_weather = false      # opt-in, see below
weather_city = ""         # e.g. "Oslo"
music_wind = false        # opt-in, see below
```

**Example:** a light snowfall that never melts, plus real weather:

```toml
intensity = "light"
melt_rate = 0

[features]
real_weather = true
weather_city = "Oslo"
```

---

## Privacy

- **Typing:** flurry only counts key presses, to shake snow loose. It never looks at *which* keys
  you press.
- **Real weather** (off by default): every 15 minutes it sends only the city name you set to
  [open-meteo.com](https://open-meteo.com), a free weather service. flurry never tries to find
  your location by itself.
- **Music wind** (off by default): it reads only how loud your speakers are playing. No audio is
  recorded, stored or sent anywhere.
- Nothing else ever leaves your computer.

---

## Troubleshooting

| Problem | Fix |
|---|---|
| **"no compositing manager is running"** | Transparent windows need a compositor. Cinnamon, GNOME, KDE and XFCE have one; on XFCE turn it on in *Window Manager Tweaks → Compositor*. On a bare window manager run `picom &`. |
| **Snow falls in front of my windows** instead of behind them | Your window manager doesn't keep flurry's layer below your apps. Set `layering = "mask"` (or `"front"`) in the settings and restart flurry. |
| **No ❄ in the tray on GNOME** | Install the *AppIndicator* GNOME extension, or use the terminal commands. |
| **Super + Shift + S does nothing** | Another program already uses that shortcut. Change `hotkey` in the settings, or bind `flurry toggle` to a key in your keyboard settings. |
| **The snow stopped** | It may have paused itself (fullscreen window, screen sharing, Do Not Disturb). `flurry status` tells you why. |
| **Wayland: no snow on windows** | flurry knows where windows are on KDE Plasma, Sway and Hyprland. On other compositors, snow piles only at the bottom of the screen. |
| **Wayland: Super + Shift + S does nothing** | Wayland doesn't let apps grab global shortcuts. Bind `flurry toggle` to a key in your desktop's keyboard settings. |
| **Something else** | Run `RUST_LOG=debug flurry` in a terminal to see what it detects: monitors, taskbars, windows. |

---

## For developers

- `./check.sh` runs formatting, lint (clippy) and all tests.
- `flurry render-test --frames 60 --warmup 600 --out frames/` simulates without a display and
  writes PNG frames.
- The design is in `PLAN.md`. Every decision and measurement is logged in `DECISIONS.md`.

## License

MIT, see `LICENSE`. Third-party credits are in `THIRD_PARTY.md`.
