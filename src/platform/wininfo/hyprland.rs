//! Hyprland: `j/clients`, `j/monitors` (ten times a second) and
//! `j/cursorpos` (thirty times a second) over the `.socket.sock` request
//! socket. Each request is its own connection; Hyprland closes it after
//! answering.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{id_of, Shared, Snapshot};
use crate::geom::{AppWindow, Rect};

const CURSOR_POLL: Duration = Duration::from_millis(33);
const WINDOWS_EVERY: Duration = Duration::from_millis(100);

fn socket() -> Result<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").context("not Hyprland")?;
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    let new = PathBuf::from(runtime)
        .join("hypr")
        .join(&sig)
        .join(".socket.sock");
    if new.exists() {
        return Ok(new);
    }
    // Hyprland before 0.40 kept it in /tmp.
    Ok(PathBuf::from("/tmp/hypr").join(&sig).join(".socket.sock"))
}

fn ask(path: &PathBuf, what: &str) -> Result<Vec<u8>> {
    let mut c = UnixStream::connect(path)?;
    c.set_read_timeout(Some(Duration::from_secs(1)))?;
    c.write_all(what.as_bytes())?;
    let mut out = Vec::new();
    c.read_to_end(&mut out)?;
    Ok(out)
}

pub fn start(shared: Shared) -> Result<Option<Box<dyn Send>>> {
    let path = socket()?;
    shared.publish(snapshot(&path)?);
    std::thread::Builder::new()
        .name("hyprland-ipc".into())
        .spawn(move || {
            let mut windows_at = Instant::now();
            loop {
                std::thread::sleep(CURSOR_POLL);
                if let Ok(Ok(p)) =
                    ask(&path, "j/cursorpos").map(|b| serde_json::from_slice::<Pos>(&b))
                {
                    shared.set_cursor(Some((p.x, p.y)));
                }
                if windows_at.elapsed() >= WINDOWS_EVERY {
                    windows_at = Instant::now();
                    match snapshot(&path) {
                        Ok(mut s) => {
                            s.cursor = shared.cursor();
                            shared.publish(s);
                        }
                        Err(e) => log::debug!("hyprland ipc: {e:#}"),
                    }
                }
            }
        })?;
    Ok(None)
}

#[derive(Debug, Deserialize)]
struct Pos {
    x: f32,
    y: f32,
}

#[derive(Debug, Deserialize)]
struct WorkspaceRef {
    id: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Client {
    address: String,
    #[serde(default = "yes")]
    mapped: bool,
    #[serde(default)]
    hidden: bool,
    at: [i32; 2],
    size: [i32; 2],
    workspace: WorkspaceRef,
    #[serde(default)]
    floating: bool,
    /// A bool in older Hyprland, a mode number (0 = none) in newer.
    #[serde(default)]
    fullscreen: serde_json::Value,
    #[serde(default)]
    pid: i64,
    /// 0 = focused most recently.
    #[serde(default = "far", rename = "focusHistoryID")]
    focus_history_id: i64,
}

fn yes() -> bool {
    true
}

fn far() -> i64 {
    i64::MAX
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Monitor {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    #[serde(default = "one")]
    scale: f32,
    active_workspace: WorkspaceRef,
    special_workspace: Option<WorkspaceRef>,
    /// Space panels take: left, top, right, bottom.
    #[serde(default)]
    reserved: [i32; 4],
}

fn one() -> f32 {
    1.0
}

fn snapshot(path: &PathBuf) -> Result<Snapshot> {
    let clients: Vec<Client> = serde_json::from_slice(&ask(path, "j/clients")?)?;
    let monitors: Vec<Monitor> = serde_json::from_slice(&ask(path, "j/monitors")?)?;
    Ok(from_json(&clients, &monitors))
}

fn from_json(clients: &[Client], monitors: &[Monitor]) -> Snapshot {
    let shown: Vec<i64> = monitors
        .iter()
        .flat_map(|m| {
            std::iter::once(m.active_workspace.id).chain(
                m.special_workspace
                    .as_ref()
                    .map(|w| w.id)
                    .filter(|&id| id != 0),
            )
        })
        .collect();
    let work_areas = monitors
        .iter()
        .map(|m| {
            // Monitor sizes are in pixels; positions and windows are logical.
            let s = if m.scale > 0.0 { m.scale } else { 1.0 };
            let (w, h) = ((m.width as f32 / s) as i32, (m.height as f32 / s) as i32);
            let [l, t, r, b] = m.reserved;
            Rect::new(
                m.x + l,
                m.y + t,
                (w - l - r).max(0) as u32,
                (h - t - b).max(0) as u32,
            )
        })
        .collect();
    let mut s = Snapshot {
        work_areas,
        ..Snapshot::default()
    };
    // Tiled below floating; within each, least recently focused first.
    let mut order: Vec<&Client> = clients.iter().collect();
    order.sort_by_key(|c| (c.floating, std::cmp::Reverse(c.focus_history_id)));
    for c in order {
        let id = id_of(&c.address);
        let full = match &c.fullscreen {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
            _ => false,
        };
        if c.focus_history_id == 0 {
            s.active = Some(id);
            s.fullscreen = full;
        }
        if c.mapped && !c.hidden && shown.contains(&c.workspace.id) {
            s.any_fullscreen |= full;
            s.windows.push(AppWindow {
                id,
                rect: Rect::new(
                    c.at[0],
                    c.at[1],
                    c.size[0].max(0) as u32,
                    c.size[1].max(0) as u32,
                ),
                collects: !full,
                pid: u32::try_from(c.pid).ok().filter(|&p| p > 0),
            });
        } else {
            s.hidden.push(id);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_clients_and_monitors() {
        let clients: Vec<Client> = serde_json::from_str(
            r#"[
            {"address":"0x5a1","mapped":true,"hidden":false,"at":[10,40],"size":[900,600],
             "workspace":{"id":1,"name":"1"},"floating":true,"fullscreen":0,"pid":77,"focusHistoryID":0},
            {"address":"0x5a2","mapped":true,"hidden":false,"at":[0,40],"size":[1920,1000],
             "workspace":{"id":1,"name":"1"},"floating":false,"fullscreen":false,"pid":78,"focusHistoryID":1},
            {"address":"0x5a3","mapped":true,"hidden":false,"at":[0,40],"size":[800,800],
             "workspace":{"id":2,"name":"2"},"floating":false,"fullscreen":0,"pid":79,"focusHistoryID":2}
            ]"#,
        )
        .unwrap();
        let monitors: Vec<Monitor> = serde_json::from_str(
            r#"[{"name":"DP-1","x":0,"y":0,"width":3840,"height":2160,"scale":2.0,
                 "activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""},
                 "reserved":[0,30,0,0]}]"#,
        )
        .unwrap();
        let s = from_json(&clients, &monitors);
        let ids: Vec<u64> = s.windows.iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![0x5a2, 0x5a1], "tiled below floating");
        assert_eq!(s.hidden, vec![0x5a3]);
        assert_eq!(s.active, Some(0x5a1));
        assert_eq!(s.windows[1].pid, Some(77));
        assert_eq!(
            s.work_areas,
            vec![Rect::new(0, 30, 1920, 1050)],
            "logical, minus the bar"
        );
    }
}
