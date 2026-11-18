//! Things that should pause the snow or slow it down, watched on background
//! threads and reported to the main loop as [`Command::Auto`]:
//! - sleep (logind `PrepareForSleep`, system bus);
//! - screen locked (logind session `LockedHint`, system bus) or blanked
//!   (`ActiveChanged` of the Cinnamon/freedesktop screensaver, session bus);
//! - Do Not Disturb (Cinnamon gsetting `display-notifications = false`).
//!
//! Battery state is read from sysfs by the main loop ([`on_battery`]).

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};

use crate::ipc::{Command, Request};

/// Why the snow is paused automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PauseReason {
    Sleep,
    Locked,
    DoNotDisturb,
    Fullscreen,
}

impl PauseReason {
    pub fn name(self) -> &'static str {
        match self {
            PauseReason::Sleep => "sleep",
            PauseReason::Locked => "screen locked",
            PauseReason::DoNotDisturb => "do not disturb",
            PauseReason::Fullscreen => "fullscreen",
        }
    }
}

fn send(tx: &Sender<Request>, cmd: Command) -> bool {
    tx.send(Request::fire(cmd)).is_ok()
}

/// Start all watchers. Each one that can't start logs why and is skipped.
/// The returned guard stops the DND monitor process when dropped.
pub fn watch(tx: &Sender<Request>, dnd: bool) -> Watchers {
    let spawn = |name: &'static str, tx: Sender<Request>, f: fn(Sender<Request>) -> Result<()>| {
        let res = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Err(e) = f(tx) {
                    log::info!("{name} watcher stopped: {e:#}");
                }
            });
        if let Err(e) = res {
            log::warn!("cannot start {name} watcher: {e}");
        }
    };
    spawn("sleep", tx.clone(), watch_sleep);
    spawn("lock", tx.clone(), watch_lock);
    spawn("screensaver", tx.clone(), watch_screensaver);
    let dnd_child = if dnd {
        watch_dnd(tx.clone()).unwrap_or_else(|e| {
            log::info!("Do Not Disturb isn't watched: {e:#}");
            None
        })
    } else {
        None
    };
    Watchers { dnd_child }
}

pub struct Watchers {
    dnd_child: Option<Child>,
}

impl Drop for Watchers {
    fn drop(&mut self) {
        if let Some(mut c) = self.dnd_child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn signal_rule(interface: &'static str, member: &'static str) -> Result<zbus::MatchRule<'static>> {
    Ok(zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(interface)?
        .member(member)?
        .build())
}

/// logind: pause before sleeping; on wake, resume and redraw.
fn watch_sleep(tx: Sender<Request>) -> Result<()> {
    let conn = zbus::blocking::Connection::system().context("no system bus")?;
    let rule = signal_rule("org.freedesktop.login1.Manager", "PrepareForSleep")?;
    log::debug!("watching logind for sleep");
    for msg in zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)? {
        let going_to_sleep: bool = msg?.body().deserialize()?;
        log::info!(
            "{}",
            if going_to_sleep {
                "going to sleep"
            } else {
                "woke up"
            }
        );
        if !send(&tx, Command::Auto(PauseReason::Sleep, going_to_sleep)) {
            break;
        }
        if !going_to_sleep && !send(&tx, Command::Wake) {
            break;
        }
    }
    Ok(())
}

const LOGIN1: &str = "org.freedesktop.login1";
const SESSION_IFACE: &str = "org.freedesktop.login1.Session";

/// logind's `LockedHint` for our session: now, then on every change.
fn watch_lock(tx: Sender<Request>) -> Result<()> {
    let conn = zbus::blocking::Connection::system().context("no system bus")?;
    // $XDG_SESSION_ID works even when flurry wasn't started inside the
    // session's process tree (e.g. from an IDE or ssh); fall back to our PID.
    let manager_call = |method: &str, arg: &str| {
        conn.call_method(
            Some(LOGIN1),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            method,
            &(arg),
        )
    };
    let reply = match std::env::var("XDG_SESSION_ID") {
        Ok(id) => manager_call("GetSession", &id),
        Err(_) => Err(zbus::Error::Failure("no XDG_SESSION_ID".into())),
    };
    let reply = match reply {
        Ok(r) => r,
        Err(_) => conn.call_method(
            Some(LOGIN1),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "GetSessionByPID",
            &(std::process::id()),
        )?,
    };
    let session: zbus::zvariant::OwnedObjectPath = reply.body().deserialize()?;
    let get = conn.call_method(
        Some(LOGIN1),
        session.as_str(),
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(SESSION_IFACE, "LockedHint"),
    )?;
    let locked: zbus::zvariant::OwnedValue = get.body().deserialize()?;
    let locked = bool::try_from(locked).unwrap_or(false);
    log::debug!("logind session {}: locked = {locked}", session.as_str());
    send(&tx, Command::Auto(PauseReason::Locked, locked));
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .path(session.as_str())?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .build();
    type Changed = (
        String,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        Vec<String>,
    );
    for msg in zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)? {
        let (iface, changed, _): Changed = msg?.body().deserialize()?;
        let Some(v) = changed.get("LockedHint").filter(|_| iface == SESSION_IFACE) else {
            continue;
        };
        let locked = bool::try_from(v.clone()).unwrap_or(false);
        log::info!("screen {}", if locked { "locked" } else { "unlocked" });
        if !send(&tx, Command::Auto(PauseReason::Locked, locked)) {
            break;
        }
    }
    Ok(())
}

/// Screensavers that announce `ActiveChanged(bool)` (screen blanked).
/// Only their changes are used: on Mint, Cinnamon's `GetActive` was seen
/// stuck at `true` in a long-unlocked session, so it isn't asked.
const SCREENSAVERS: [&str; 2] = ["org.cinnamon.ScreenSaver", "org.freedesktop.ScreenSaver"];

fn watch_screensaver(tx: Sender<Request>) -> Result<()> {
    let conn = zbus::blocking::Connection::session().context("no session bus")?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .member("ActiveChanged")?
        .build();
    for msg in zbus::blocking::MessageIterator::for_match_rule(rule, &conn, None)? {
        let msg = msg?;
        let header = msg.header();
        let from_screensaver = header
            .interface()
            .is_some_and(|i| SCREENSAVERS.contains(&i.as_str()));
        if !from_screensaver {
            continue;
        }
        let active: bool = msg.body().deserialize()?;
        log::info!("screensaver {}", if active { "active" } else { "inactive" });
        if !send(&tx, Command::Auto(PauseReason::Locked, active)) {
            break;
        }
    }
    Ok(())
}

const DND_SCHEMA: &str = "org.cinnamon.desktop.notifications";
const DND_KEY: &str = "display-notifications";

/// `display-notifications = false` means Do Not Disturb is on.
fn parse_dnd(line: &str) -> Option<bool> {
    match line.trim().rsplit(':').next()?.trim() {
        "false" => Some(true),
        "true" => Some(false),
        _ => None,
    }
}

/// Cinnamon's DND: read it now, then follow `gsettings monitor`.
fn watch_dnd(tx: Sender<Request>) -> Result<Option<Child>> {
    let out = Process::new("gsettings")
        .args(["get", DND_SCHEMA, DND_KEY])
        .stderr(Stdio::null())
        .output()
        .context("gsettings not available")?;
    anyhow::ensure!(
        out.status.success(),
        "no {DND_SCHEMA} {DND_KEY} (not Cinnamon?)"
    );
    if let Some(on) = parse_dnd(&String::from_utf8_lossy(&out.stdout)) {
        send(&tx, Command::Auto(PauseReason::DoNotDisturb, on));
    }
    let mut child = Process::new("gsettings")
        .args(["monitor", DND_SCHEMA, DND_KEY])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot run gsettings monitor")?;
    let stdout = child.stdout.take().context("no stdout")?;
    std::thread::Builder::new()
        .name("dnd".into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(on) = parse_dnd(&line) {
                    log::info!("Do Not Disturb {}", if on { "on" } else { "off" });
                    if !send(&tx, Command::Auto(PauseReason::DoNotDisturb, on)) {
                        break;
                    }
                }
            }
        })?;
    log::debug!("watching {DND_SCHEMA} {DND_KEY}");
    Ok(Some(child))
}

/// True when running on battery: a mains adapter exists and is offline, or
/// (no adapter listed) a battery reports `Discharging`.
pub fn on_battery(power_supply: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(power_supply) else {
        return false;
    };
    let read = |dir: &Path, f: &str| {
        std::fs::read_to_string(dir.join(f))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    };
    let (mut mains, mut mains_online, mut discharging) = (false, false, false);
    for e in entries.flatten() {
        let dir = e.path();
        match read(&dir, "type").as_str() {
            "Mains" => {
                mains = true;
                mains_online |= read(&dir, "online") == "1";
            }
            "Battery" => discharging |= read(&dir, "status") == "Discharging",
            _ => {}
        }
    }
    if mains {
        !mains_online
    } else {
        discharging
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnd_lines() {
        assert_eq!(parse_dnd("false\n"), Some(true));
        assert_eq!(parse_dnd("display-notifications: true"), Some(false));
        assert_eq!(parse_dnd("display-notifications: false"), Some(true));
        assert_eq!(parse_dnd("garbage"), None);
    }

    fn supply(root: &Path, name: &str, files: &[(&str, &str)]) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        for (f, v) in files {
            std::fs::write(d.join(f), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn battery_detection() {
        let root = std::env::temp_dir().join(format!("flurry-ps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        supply(
            &root,
            "BAT0",
            &[("type", "Battery"), ("status", "Discharging")],
        );
        assert!(on_battery(&root), "battery only, discharging");
        supply(&root, "ADP0", &[("type", "Mains"), ("online", "1")]);
        assert!(!on_battery(&root), "plugged in");
        supply(&root, "ADP0", &[("type", "Mains"), ("online", "0")]);
        assert!(on_battery(&root), "unplugged");
        std::fs::remove_dir_all(&root).unwrap();
        assert!(!on_battery(&root), "no sysfs: assume mains (desktop)");
    }
}
