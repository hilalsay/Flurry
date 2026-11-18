//! Display backends. The `Backend` trait arrives with the first backend
//! (phase 3, X11), once its `present` signature can be exercised.

pub mod wayland;
pub mod wininfo;
pub mod x11;
