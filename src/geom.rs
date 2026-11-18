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

/// A toplevel window. `Inputs::windows` is ordered bottom-most first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowRect {
    pub id: u64,
    pub rect: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CursorState {
    pub x: f32,
    pub y: f32,
    pub left_down: bool,
    /// True while the configured carry modifier (e.g. Alt) is held.
    pub modifier_down: bool,
}

/// Everything the simulation learns about the outside world each frame.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub outputs: Vec<OutputInfo>,
    /// Stacking order: top-most window last.
    pub windows: Vec<WindowRect>,
    /// Panels, docks, taskbars (id = window id).
    pub obstacles: Vec<WindowRect>,
    /// `None` when the backend cannot see the cursor (most Wayland desktops).
    pub cursor: Option<CursorState>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
