//! Real weather mode (feature 5, opt-in): every 15 minutes, ask Open-Meteo
//! (free, no API key) for the current weather in the city set in the
//! config, and let the snow follow it:
//! - snowfall sets the intensity, rain or no precipitation → gentle (light);
//! - wind speed and direction set the wind.
//!
//! The city comes only from the config; flurry never looks up the user's
//! location. The last result is cached in `~/.cache/flurry/weather.json`
//! and used when offline. HTTP goes through the system `curl`.

use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Intensity;
use crate::ipc::{Command, Request};

const REFRESH: Duration = Duration::from_secs(15 * 60);
const RETRY: Duration = Duration::from_secs(2 * 60);
/// A cached result older than this is not used at startup.
const CACHE_MAX_AGE: u64 = 6 * 3600;

/// What the weather does to the snow.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    pub intensity: Intensity,
    /// Signed wind strength multiplier (negative = blowing to the left).
    pub wind: f32,
}

/// The `current` block of an Open-Meteo forecast.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
struct Current {
    /// cm in the last hour.
    snowfall: f32,
    /// WMO weather code.
    weather_code: u32,
    /// km/h.
    wind_speed_10m: f32,
    /// Degrees the wind comes *from* (meteorological).
    wind_direction_10m: f32,
}

/// WMO codes for heavy snowfall / heavy snow showers.
fn heavy_snow_code(code: u32) -> bool {
    matches!(code, 75 | 86)
}

fn effect_of(c: &Current) -> Effect {
    let intensity = if c.snowfall >= 1.0 {
        Intensity::Heavy
    } else if c.snowfall >= 0.2 || heavy_snow_code(c.weather_code) {
        Intensity::Medium
    } else {
        // Light snow, rain, or a dry sky: gentle.
        Intensity::Light
    };
    // Wind from the west (270°) blows to the right (+x).
    let towards_right = -c.wind_direction_10m.to_radians().sin();
    let strength = (c.wind_speed_10m / 15.0).clamp(0.0, 4.0);
    Effect {
        intensity,
        wind: strength * towards_right,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cache {
    city: String,
    latitude: f64,
    longitude: f64,
    effect: Option<Effect>,
    /// Unix seconds of `effect`.
    fetched: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn cache_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "flurry").map(|d| d.cache_dir().join("weather.json"))
}

fn load_cache(path: &Path, city: &str) -> Option<Cache> {
    let c: Cache = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    (c.city == city).then_some(c)
}

fn save_cache(path: &Path, c: &Cache) {
    let write = || -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(c)?)?;
        Ok(())
    };
    if let Err(e) = write() {
        log::debug!("cannot cache weather: {e:#}");
    }
}

/// Percent-encode for a URL query value.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn get(url: &str) -> Result<String> {
    let out = Process::new("curl")
        .args(["-fsS", "--max-time", "15", url])
        .output()
        .context("curl is not installed")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[derive(Deserialize)]
struct Place {
    latitude: f64,
    longitude: f64,
    name: String,
    #[serde(default)]
    country: String,
}

#[derive(Deserialize)]
struct Places {
    #[serde(default)]
    results: Vec<Place>,
}

fn geocode(city: &str) -> Result<Place> {
    let url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?count=1&language=en&format=json&name={}",
        encode(city)
    );
    let places: Places = serde_json::from_str(&get(&url)?)?;
    places
        .results
        .into_iter()
        .next()
        .with_context(|| format!("Open-Meteo doesn't know the city {city:?}"))
}

#[derive(Deserialize)]
struct Forecast {
    current: Current,
}

fn current(lat: f64, lon: f64) -> Result<Current> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.4}&longitude={lon:.4}\
         &current=snowfall,weather_code,wind_speed_10m,wind_direction_10m"
    );
    let f: Forecast = serde_json::from_str(&get(&url)?)?;
    Ok(f.current)
}

/// Start following the weather of `city` on a background thread.
pub fn watch(city: String, tx: Sender<Request>) {
    if city.trim().is_empty() {
        log::warn!("features.real_weather is on but features.weather_city is empty");
        return;
    }
    let res = std::thread::Builder::new()
        .name("weather".into())
        .spawn(move || follow(&city, &tx));
    if let Err(e) = res {
        log::warn!("cannot start the weather thread: {e}");
    }
}

fn follow(city: &str, tx: &Sender<Request>) {
    let path = cache_path();
    let mut cache = path.as_deref().and_then(|p| load_cache(p, city));
    if let Some(c) = &cache {
        if let Some(e) = c
            .effect
            .filter(|_| now_secs().saturating_sub(c.fetched) < CACHE_MAX_AGE)
        {
            log::info!("weather for {city} (cached): {e:?}");
            if tx.send(Request::fire(Command::Weather(e))).is_err() {
                return;
            }
        }
    }
    loop {
        let wait = match refresh(city, &mut cache) {
            Ok(e) => {
                log::info!("weather for {city}: {e:?}");
                if let (Some(p), Some(c)) = (&path, &cache) {
                    save_cache(p, c);
                }
                if tx.send(Request::fire(Command::Weather(e))).is_err() {
                    return;
                }
                REFRESH
            }
            Err(e) => {
                log::info!("no weather update ({e:#}); keeping the last one");
                RETRY
            }
        };
        std::thread::sleep(wait);
    }
}

fn refresh(city: &str, cache: &mut Option<Cache>) -> Result<Effect> {
    let (lat, lon) = match cache {
        Some(c) => (c.latitude, c.longitude),
        None => {
            let p = geocode(city)?;
            log::info!(
                "weather: {city} is {}, {} ({:.2}, {:.2})",
                p.name,
                p.country,
                p.latitude,
                p.longitude
            );
            *cache = Some(Cache {
                city: city.to_owned(),
                latitude: p.latitude,
                longitude: p.longitude,
                effect: None,
                fetched: 0,
            });
            (p.latitude, p.longitude)
        }
    };
    let e = effect_of(&current(lat, lon)?);
    if let Some(c) = cache {
        c.effect = Some(e);
        c.fetched = now_secs();
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(snowfall: f32, speed: f32, from: f32) -> Current {
        Current {
            snowfall,
            weather_code: 0,
            wind_speed_10m: speed,
            wind_direction_10m: from,
        }
    }

    #[test]
    fn snowfall_sets_intensity() {
        assert_eq!(effect_of(&cur(0.0, 0.0, 0.0)).intensity, Intensity::Light);
        assert_eq!(effect_of(&cur(0.5, 0.0, 0.0)).intensity, Intensity::Medium);
        assert_eq!(effect_of(&cur(2.0, 0.0, 0.0)).intensity, Intensity::Heavy);
    }

    #[test]
    fn wind_direction_and_speed() {
        let west = effect_of(&cur(0.0, 30.0, 270.0));
        assert!(
            (west.wind - 2.0).abs() < 1e-3,
            "from the west blows right: {west:?}"
        );
        let east = effect_of(&cur(0.0, 15.0, 90.0));
        assert!(
            (east.wind + 1.0).abs() < 1e-3,
            "from the east blows left: {east:?}"
        );
        let north = effect_of(&cur(0.0, 20.0, 0.0));
        assert!(north.wind.abs() < 1e-3, "from the north: no sideways wind");
        assert!(effect_of(&cur(0.0, 500.0, 270.0)).wind <= 4.0);
    }

    #[test]
    fn parses_open_meteo_json() {
        let json = r#"{"latitude":59.9,"longitude":10.7,"current_units":{},
            "current":{"time":"2026-11-18T22:00","interval":900,"snowfall":0.35,
            "rain":0.0,"weather_code":73,"wind_speed_10m":12.4,"wind_direction_10m":250}}"#;
        let f: Forecast = serde_json::from_str(json).unwrap();
        assert_eq!(f.current.weather_code, 73);
        assert_eq!(effect_of(&f.current).intensity, Intensity::Medium);
        let places: Places =
            serde_json::from_str(r#"{"results":[{"latitude":1.5,"longitude":2.5,"name":"X"}]}"#)
                .unwrap();
        assert_eq!(places.results[0].name, "X");
        let none: Places = serde_json::from_str(r#"{"generationtime_ms":0.3}"#).unwrap();
        assert!(none.results.is_empty());
    }

    #[test]
    fn query_encoding() {
        assert_eq!(encode("São Paulo"), "S%C3%A3o%20Paulo");
        assert_eq!(encode("New-York_1.~"), "New-York_1.~");
    }
}
