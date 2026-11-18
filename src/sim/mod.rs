//! Pure simulation: no platform code, fully unit-testable.
//! Must not import from `platform`, `tray` or `ipc` (PLAN.md §4).

pub mod flakes;
pub mod piles;
pub mod snowballs;
