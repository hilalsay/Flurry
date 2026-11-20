//! Sway: `GET_TREE` and `GET_WORKSPACES` over `$SWAYSOCK` (the i3 IPC
//! protocol), ten times a second.
//!
//! Sway doesn't tell where the pointer is, so on Sway the cursor is known
//! only while it is over flurry's own clickable areas (snowballs).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{Shared, Snapshot};
use crate::geom::{AppWindow, Rect};

const POLL: Duration = Duration::from_millis(100);
const GET_WORKSPACES: u32 = 1;
const GET_TREE: u32 = 4;

pub fn start(shared: Shared) -> Result<Option<Box<dyn Send>>> {
    let path = std::env::var("SWAYSOCK").context("SWAYSOCK not set")?;
    // Fail now (not in the thread) if Sway can't be reached.
    let mut conn = UnixStream::connect(&path).with_context(|| format!("connect {path}"))?;
    shared.publish(snapshot(&mut conn)?);
    std::thread::Builder::new()
        .name("sway-ipc".into())
        .spawn(move || loop {
            std::thread::sleep(POLL);
            match snapshot(&mut conn) {
                Ok(s) => shared.publish(s),
                Err(e) => {
                    log::debug!("sway ipc: {e:#}; reconnecting");
                    std::thread::sleep(Duration::from_secs(1));
                    match UnixStream::connect(&path) {
                        Ok(c) => conn = c,
                        Err(_) => continue,
                    }
                }
            }
        })?;
    Ok(None)
}

fn request(conn: &mut UnixStream, kind: u32) -> Result<Vec<u8>> {
    let mut msg = b"i3-ipc".to_vec();
    msg.extend_from_slice(&0u32.to_le_bytes());
    msg.extend_from_slice(&kind.to_le_bytes());
    conn.write_all(&msg)?;
    let mut header = [0u8; 14];
    conn.read_exact(&mut header)?;
    if &header[..6] != b"i3-ipc" {
        bail!("bad reply header");
    }
    let len = u32::from_le_bytes(header[6..10].try_into()?) as usize;
    let mut body = vec![0u8; len];
    conn.read_exact(&mut body)?;
    Ok(body)
}

fn snapshot(conn: &mut UnixStream) -> Result<Snapshot> {
    let tree: Node = serde_json::from_slice(&request(conn, GET_TREE)?)?;
    let workspaces: Vec<Workspace> = serde_json::from_slice(&request(conn, GET_WORKSPACES)?)?;
    Ok(from_tree(&tree, &workspaces))
}

#[derive(Debug, Deserialize, Clone, Copy, Default)]
struct SwayRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl SwayRect {
    fn rect(self) -> Rect {
        Rect::new(
            self.x,
            self.y,
            self.width.max(0) as u32,
            self.height.max(0) as u32,
        )
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Node {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    name: Option<String>,
    rect: SwayRect,
    visible: Option<bool>,
    focused: bool,
    fullscreen_mode: u8,
    pid: Option<u32>,
    nodes: Vec<Node>,
    floating_nodes: Vec<Node>,
}

#[derive(Debug, Deserialize)]
struct Workspace {
    visible: bool,
    rect: SwayRect,
}

/// A window: a container without children that holds an app.
fn is_window(n: &Node) -> bool {
    matches!(n.kind.as_str(), "con" | "floating_con")
        && n.nodes.is_empty()
        && n.floating_nodes.is_empty()
        && (n.pid.is_some() || n.name.is_some())
}

fn from_tree(root: &Node, workspaces: &[Workspace]) -> Snapshot {
    let mut s = Snapshot {
        work_areas: workspaces
            .iter()
            .filter(|w| w.visible)
            .map(|w| w.rect.rect())
            .collect(),
        ..Snapshot::default()
    };
    // Tiled windows first, floating ones above them (bottom-most first).
    let (mut tiled, mut floating) = (Vec::new(), Vec::new());
    walk(root, false, &mut tiled, &mut floating);
    for n in tiled.iter().chain(&floating) {
        let full = n.fullscreen_mode > 0;
        if n.focused {
            s.active = Some(n.id);
            s.fullscreen = full;
        }
        if n.visible == Some(true) {
            s.any_fullscreen |= full;
            s.windows.push(AppWindow {
                id: n.id,
                rect: n.rect.rect(),
                collects: !full,
                pid: n.pid,
            });
        } else {
            s.hidden.push(n.id);
        }
    }
    s
}

fn walk<'a>(
    n: &'a Node,
    in_floating: bool,
    tiled: &mut Vec<&'a Node>,
    floating: &mut Vec<&'a Node>,
) {
    if is_window(n) {
        if in_floating {
            floating.push(n);
        } else {
            tiled.push(n);
        }
        return;
    }
    for c in &n.nodes {
        walk(c, in_floating, tiled, floating);
    }
    for c in &n.floating_nodes {
        walk(c, true, tiled, floating);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = r#"{"id":1,"type":"root","rect":{"x":0,"y":0,"width":1920,"height":1080},
      "nodes":[{"id":2,"type":"output","name":"HEADLESS-1","rect":{"x":0,"y":0,"width":1920,"height":1080},
        "nodes":[
          {"id":3,"type":"workspace","name":"1","rect":{"x":0,"y":0,"width":1920,"height":1050},
           "nodes":[
             {"id":10,"type":"con","name":"foot","pid":100,"visible":true,"focused":false,
              "rect":{"x":0,"y":0,"width":960,"height":1050},"nodes":[],"floating_nodes":[]},
             {"id":11,"type":"con","name":"term","pid":101,"visible":true,"focused":true,"fullscreen_mode":0,
              "rect":{"x":960,"y":0,"width":960,"height":1050},"nodes":[],"floating_nodes":[]}],
           "floating_nodes":[
             {"id":12,"type":"floating_con","name":"calc","pid":102,"visible":true,
              "rect":{"x":500,"y":400,"width":300,"height":200},"nodes":[],"floating_nodes":[]}]},
          {"id":4,"type":"workspace","name":"2","rect":{"x":0,"y":0,"width":1920,"height":1050},
           "nodes":[{"id":13,"type":"con","name":"mail","pid":103,"visible":false,
              "rect":{"x":0,"y":0,"width":1920,"height":1050},"nodes":[],"floating_nodes":[]}],
           "floating_nodes":[]}]}]}"#;

    const WORKSPACES: &str = r#"[{"num":1,"visible":true,"rect":{"x":0,"y":0,"width":1920,"height":1050}},
        {"num":2,"visible":false,"rect":{"x":0,"y":0,"width":1920,"height":1050}}]"#;

    #[test]
    fn reads_windows_stacking_and_work_area() {
        let tree: Node = serde_json::from_str(TREE).unwrap();
        let ws: Vec<Workspace> = serde_json::from_str(WORKSPACES).unwrap();
        let s = from_tree(&tree, &ws);
        let ids: Vec<u64> = s.windows.iter().map(|w| w.id).collect();
        assert_eq!(ids, vec![10, 11, 12], "tiled first, floating on top");
        assert_eq!(s.windows[2].rect, Rect::new(500, 400, 300, 200));
        assert_eq!(s.windows[0].pid, Some(100));
        assert_eq!(s.hidden, vec![13], "other workspace");
        assert_eq!(s.active, Some(11));
        assert!(!s.fullscreen);
        assert_eq!(s.work_areas, vec![Rect::new(0, 0, 1920, 1050)], "bar below");
    }
}
