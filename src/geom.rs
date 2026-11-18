//! Plain geometry and per-frame input types shared by `sim` and `platform`.
//!
//! These live outside both so that `sim/` never imports from `platform/`
//! (see PLAN.md §4 "Key rule"). The platform layer builds them; the
//! simulation only reads them.

/// Axis-aligned rectangle in global (virtual-desktop) pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add_unsigned(self.w)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add_unsigned(self.h)
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }

    /// Overlapping area of two rects, or `None` if they don't overlap.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
    }
}

/// One monitor, in global coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputInfo {
    /// Connector name, e.g. `"DP-1"`. Used by `monitors = [...]` in config.
    pub name: String,
    pub rect: Rect,
    /// Buffer scale (1.0 on X11; may be fractional on Wayland).
    pub scale: f32,
}

/// A panel/dock (or any window) and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRect {
    pub id: u64,
    pub rect: Rect,
}

/// A visible app window on the current workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppWindow {
    pub id: u64,
    /// The visible frame (title bar included, invisible shadows excluded).
    pub rect: Rect,
    /// Snow may settle on its top edge (normal, not maximized/fullscreen).
    pub collects: bool,
    /// The owning process (`_NET_WM_PID`), if known.
    pub pid: Option<u32>,
}

/// The mouse pointer, global coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CursorState {
    pub x: f32,
    pub y: f32,
    pub left_down: bool,
    pub alt: bool,
    pub super_: bool,
}

/// Everything the simulation learns about the outside world each frame.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub outputs: Vec<OutputInfo>,
    /// Visible app windows in stacking order, top-most last.
    pub windows: Vec<AppWindow>,
    /// Windows that exist but are hidden (minimized, other workspace).
    pub hidden_windows: Vec<u64>,
    /// CPU load of each window's program (1.0 = one core), by window id.
    pub window_heat: std::collections::HashMap<u64, f32>,
    /// The focused window.
    pub active_window: Option<u64>,
    /// Keys pressed since the last step (counted only, never which keys).
    pub keys_pressed: u32,
    /// Panels, docks, taskbars (id = window id).
    pub obstacles: Vec<WindowRect>,
    /// `None` when the backend cannot see the cursor (most Wayland desktops).
    pub cursor: Option<CursorState>,
}

/// `rects` minus `holes`, as non-overlapping rects.
pub fn subtract(rects: &[Rect], holes: &[Rect]) -> Vec<Rect> {
    let mut out: Vec<Rect> = rects.iter().copied().filter(|r| !r.is_empty()).collect();
    for hole in holes {
        let mut next = Vec::with_capacity(out.len());
        for r in out {
            let Some(i) = r.intersect(hole) else {
                next.push(r);
                continue;
            };
            // Up to four pieces: above, below, left, right of the hole.
            let pieces = [
                Rect::new(r.x, r.y, r.w, (i.y - r.y) as u32),
                Rect::new(r.x, i.bottom(), r.w, (r.bottom() - i.bottom()) as u32),
                Rect::new(r.x, i.y, (i.x - r.x) as u32, i.h),
                Rect::new(i.right(), i.y, (r.right() - i.right()) as u32, i.h),
            ];
            next.extend(pieces.into_iter().filter(|p| !p.is_empty()));
        }
        out = next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtract_cuts_holes() {
        let r = Rect::new(0, 0, 10, 10);
        assert_eq!(subtract(&[r], &[]), [r]);
        assert!(subtract(&[r], &[Rect::new(-5, -5, 20, 20)]).is_empty());
        let parts = subtract(&[r], &[Rect::new(3, 3, 4, 4)]);
        let area: u32 = parts.iter().map(|p| p.w * p.h).sum();
        assert_eq!(area, 100 - 16);
        for (i, a) in parts.iter().enumerate() {
            assert!(a.intersect(&Rect::new(3, 3, 4, 4)).is_none());
            for b in &parts[i + 1..] {
                assert!(a.intersect(b).is_none());
            }
        }
    }

    #[test]
    fn intersect_overlapping() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Some(Rect::new(5, 5, 5, 5)));
    }

    #[test]
    fn intersect_touching_edges_is_none() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(10, 0, 10, 10);
        assert_eq!(a.intersect(&b), None);
    }

    #[test]
    fn contains_is_half_open() {
        let r = Rect::new(-5, -5, 10, 10);
        assert!(r.contains(-5, -5));
        assert!(r.contains(4, 4));
        assert!(!r.contains(5, 0));
    }
}
