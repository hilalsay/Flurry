//! Music makes wind (feature 7, opt-in): read the level of what the
//! speakers play (the default sink's monitor, through `parec`) and turn
//! bass hits into wind gusts. Only a loudness number is computed; no audio
//! is stored or sent anywhere.

use std::io::Read;
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};

use crate::ipc::{Command, Request};

const RATE: u32 = 8000;
/// Samples per analysis window (50 ms).
const WINDOW: usize = 400;
/// Low-pass corner for "bass" (Hz).
const BASS_HZ: f32 = 150.0;
/// A window this much louder than the running average is a hit...
const HIT_RATIO: f32 = 1.6;
/// ...if it is also louder than this (of 32767), so silence stays still.
const NOISE_FLOOR: f32 = 300.0;
/// At most one hit per this many windows (350 ms).
const MIN_GAP: u32 = 7;

/// Finds bass hits in 16-bit mono samples.
#[derive(Debug, Clone)]
pub struct BassDetector {
    alpha: f32,
    /// Two one-pole low-pass stages (12 dB/octave), so treble really drops out.
    low: [f32; 2],
    avg: f32,
    since_hit: u32,
    buf: Vec<f32>,
}

impl Default for BassDetector {
    fn default() -> Self {
        Self {
            alpha: 1.0 - (-std::f32::consts::TAU * BASS_HZ / RATE as f32).exp(),
            low: [0.0; 2],
            avg: 0.0,
            since_hit: MIN_GAP,
            buf: Vec::with_capacity(WINDOW),
        }
    }
}

impl BassDetector {
    /// Feed samples; returns the strength (0.2..=2) of each hit found.
    pub fn push(&mut self, samples: &[i16]) -> Vec<f32> {
        let mut hits = Vec::new();
        for &s in samples {
            self.low[0] += self.alpha * (f32::from(s) - self.low[0]);
            self.low[1] += self.alpha * (self.low[0] - self.low[1]);
            self.buf.push(self.low[1]);
            if self.buf.len() < WINDOW {
                continue;
            }
            let rms = (self.buf.iter().map(|v| v * v).sum::<f32>() / WINDOW as f32).sqrt();
            self.buf.clear();
            self.since_hit = self.since_hit.saturating_add(1);
            let loud = rms > NOISE_FLOOR && rms > self.avg * HIT_RATIO;
            if loud && self.since_hit >= MIN_GAP {
                self.since_hit = 0;
                hits.push((rms / self.avg.max(1.0) - 1.0).clamp(0.2, 2.0));
            }
            // Running average over ~2 s.
            self.avg += (rms - self.avg) * 0.025;
        }
        hits
    }
}

/// Start listening; returns the `parec` child (kill it to stop).
pub fn watch(tx: Sender<Request>) -> Result<Child> {
    let mut child = Process::new("parec")
        .args([
            "--device=@DEFAULT_MONITOR@",
            "--format=s16le",
            &format!("--rate={RATE}"),
            "--channels=1",
            "--latency-msec=50",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot run parec (PulseAudio/PipeWire tools)")?;
    let mut out = child.stdout.take().context("no stdout")?;
    std::thread::Builder::new()
        .name("music".into())
        .spawn(move || {
            let mut det = BassDetector::default();
            let mut bytes = vec![0u8; WINDOW * 2];
            while out.read_exact(&mut bytes).is_ok() {
                let samples: Vec<i16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|&b| i16::from_le_bytes(b))
                    .collect();
                for strength in det.push(&samples) {
                    if tx.send(Request::fire(Command::Gust(strength))).is_err() {
                        return;
                    }
                }
            }
            log::info!("music input ended");
        })?;
    log::info!("music makes wind: listening to the speakers' level");
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, amp: f32, secs: f32) -> Vec<i16> {
        (0..(secs * RATE as f32) as usize)
            .map(|i| (amp * (std::f32::consts::TAU * hz * i as f32 / RATE as f32).sin()) as i16)
            .collect()
    }

    fn quiet(secs: f32) -> Vec<i16> {
        vec![0; (secs * RATE as f32) as usize]
    }

    #[test]
    fn bass_beats_are_hits() {
        let mut d = BassDetector::default();
        let mut hits = 0;
        for _ in 0..8 {
            hits += d.push(&tone(60.0, 12000.0, 0.15)).len();
            hits += d.push(&quiet(0.35)).len();
        }
        assert!(hits >= 6, "{hits} hits for 8 kick drums");
    }

    #[test]
    fn silence_and_treble_are_not() {
        let mut d = BassDetector::default();
        assert!(d.push(&quiet(3.0)).is_empty());
        let mut hits = 0;
        for _ in 0..8 {
            hits += d.push(&tone(2500.0, 12000.0, 0.15)).len();
            hits += d.push(&quiet(0.35)).len();
        }
        assert_eq!(hits, 0, "hi-hats don't make wind");
    }
}
