# flurry — snow on your Linux desktop

![Snow falling behind a window, piling up on its title bar and on the panel, with icicles](docs/flurry.gif)

Snow falls **behind** your windows and piles up **on top of them**, on the panel and along the
bottom of the screen. It melts, slides off, drips and grows icicles. Drag a window fast and its
snow flies off. Sweep the mouse through a drift to roll a snowball, carry it with **Alt**, and stack
three into a snowman.

Made for **Linux Mint (Cinnamon)** and other X11 desktops with a compositor: GNOME on Xorg, XFCE,
MATE, KDE Plasma X11. Wayland support is planned.

## Install

### Linux Mint, Ubuntu, Debian (.deb)

Build the package (needs [Rust](https://rustup.rs)), then install it:

```bash
git clone https://github.com/flurry-snow/flurry && cd flurry
./scripts/build-deb.sh                      # prints target/flurry_<version>_amd64.deb
sudo apt install ./target/flurry_*_amd64.deb
```

### Any distribution (cargo)

```bash
cargo install --path .        # installs ~/.cargo/bin/flurry
```

Arch Linux: `packaging/PKGBUILD`. AppImage: `scripts/build-appimage.sh` (needs `appimagetool`).

## Use

Start it from the menu (**Flurry**) or a terminal:

```bash
flurry &                      # start; a snowflake appears in the tray
flurry --install-autostart    # start it at every login (--remove-autostart to undo)
```

| Command | What it does |
|---|---|
| `flurry toggle` | pause / resume (also **Super+Shift+S**, or click the tray icon) |
| `flurry intensity light\|medium\|heavy` | how much snow |
| `flurry fps 60` | smoother (30 is the default and uses half the CPU) |
| `flurry clear` | sweep away all settled snow |
| `flurry status` | what it's doing right now |
| `flurry reload` | re-read the config file |
| `flurry quit` | stop |

The tray menu has the same controls.

### Playing with the snow

- **Roll a snowball**: move the mouse sideways through a snow pile (on the panel or a window top).
  Keep pushing and it grows.
- **Carry it**: point at the top half of a snowball (the cursor turns into a hand), then click and
  drag it. Let go to drop or throw it. A long fall makes it *poof*. With **Alt** held you can
  grab it anywhere.
- **Snowman**: stack any three balls on top of each other. They become a snowman with a face, a
  scarf and stick arms (the snow rearranges so the biggest ball ends up at the bottom).
- **Write in the snow**: hold **Alt** and drag through a deep pile.
- **Snow globe**: shake the mouse fast from side to side.
- **Frost**: leave the mouse alone for a few minutes. Move it to wipe the frost away.
- Typing knocks little clumps of snow off the window you type in, and busy programs (a compiler, a
  game) melt their window's snow faster.

flurry pauses by itself while a window is fullscreen, the screen is locked or shared, Do Not
Disturb is on, or the computer sleeps. On battery it uses half the flakes.

## Configure

`~/.config/flurry/config.toml` (every key is optional; these are the defaults):

```toml
intensity = "medium"      # light | medium | heavy
# flake_count = 300       # per 1920×1080 of screen; overrides intensity
fall_speed = 1.0
wind = 1.0
max_pile_height = 40      # px
melt_rate = 1.0
fps = 30
pile_on_windows = true
interaction = true        # snowballs etc.
carry_modifier = "alt"    # none | alt | super: grab a whole ball / write in the snow
auto_pause_fullscreen = true
monitors = "all"          # or ["DP-1", "eDP-1"]
layering = "auto"         # auto | two-window | mask | desktop | front
hotkey = "super+shift+s"  # "" to disable

[features]
hot_apps_melt = true
icicles = true
typing_shakes_snow = true
frost = true
frost_idle_minutes = 3.0
snow_globe = true
write_in_snow = true
privacy_pause = true
cross_monitor = true
real_weather = false      # opt-in: follow the real weather of weather_city (Open-Meteo)
weather_city = ""         # e.g. "Istanbul"; never detected automatically
music_wind = false        # opt-in: bass in your music makes wind
```

Apply changes with `flurry reload` (monitors, layering and the hotkey need a restart).

## Privacy

- **Typing**: flurry counts key presses to shake snow loose; it never looks at *which* keys.
- **Real weather** (off by default): sends only the city you configure to open-meteo.com.
- **Music wind** (off by default): reads the volume level of what your speakers play; no audio is
  stored or sent anywhere.
- Nothing else leaves your computer.

## Troubleshooting

- **"no compositing manager is running"**: transparency needs a compositor. Cinnamon, GNOME, KDE
  and XFCE (Window Manager Tweaks → Compositor) have one; on bare window managers run `picom &`.
- **Snow is on top of windows instead of behind them**: your window manager doesn't keep flurry's
  desktop layer below apps; flurry then falls back to drawing around windows. Try
  `layering = "mask"` or `"front"`.
- **No tray icon on GNOME**: install the *AppIndicator* extension, or use the commands above.
- **Super+Shift+S does nothing**: another program has that shortcut. Bind `flurry toggle` in your
  keyboard settings, or set `hotkey` in the config.
- `RUST_LOG=debug flurry` shows what it detects (monitors, panels, windows).

## Development

`./check.sh` (format, clippy, tests). `flurry render-test --frames 60 --warmup 600 --out frames/`
writes PNG frames of the simulation without a display. Design and decisions: `PLAN.md`,
`DECISIONS.md`.

## License

MIT, see `LICENSE`. Credits: `THIRD_PARTY.md`.
