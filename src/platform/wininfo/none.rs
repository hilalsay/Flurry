//! Compositors that don't tell where windows are (GNOME, COSMIC, others):
//! no provider thread runs, the snapshot stays empty, and snow piles only
//! at the bottom of each screen. See [`super::start`].
