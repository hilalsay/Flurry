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
