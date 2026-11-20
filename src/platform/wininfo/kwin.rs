//! KDE Plasma (Wayland): a KWin script reports the windows, the free
//! screen area and the pointer to flurry over D-Bus.
//!
//! flurry serves `org.flurry.Flurry` on the session bus, writes the script
//! to `$XDG_RUNTIME_DIR`, and loads it through KWin's `/Scripting`
//! interface; dropping the guard unloads it again. The script works with
//! the Plasma 5 and Plasma 6 scripting APIs.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::{id_of, Shared, Snapshot};
use crate::geom::{AppWindow, Rect};

const SERVICE: &str = "org.flurry.Flurry";
const PATH: &str = "/org/flurry/Flurry";
const PLUGIN: &str = "flurry-bridge";

const SCRIPT: &str = r#"
// flurry: tells flurry where windows, panels and the pointer are.
function call(method, arg1, arg2) {
    if (arg2 === undefined) {
        callDBus("org.flurry.Flurry", "/org/flurry/Flurry", "org.flurry.Flurry", method, arg1);
    } else {
        callDBus("org.flurry.Flurry", "/org/flurry/Flurry", "org.flurry.Flurry", method, arg1, arg2);
    }
}
var plasma6 = workspace.windowList !== undefined;
function allWindows() {
    if (workspace.stackingOrder !== undefined) return workspace.stackingOrder;
    return plasma6 ? workspace.windowList() : workspace.clientList();
}
function active() { return plasma6 ? workspace.activeWindow : workspace.activeClient; }
function onCurrentDesktop(w) {
    if (w.onAllDesktops) return true;
    if (w.desktops !== undefined && typeof workspace.currentDesktop === "object") {
        for (var i = 0; i < w.desktops.length; i++)
            if (w.desktops[i].id === workspace.currentDesktop.id) return true;
        return false;
    }
    return w.desktop === workspace.currentDesktop;
}
function workAreas() {
    var out = [];
    var screens = plasma6 ? workspace.screens : null;
    var n = plasma6 ? screens.length : workspace.numScreens;
    for (var i = 0; i < n; i++) {
        var a = workspace.clientArea(KWin.MaximizeArea, plasma6 ? screens[i] : i, workspace.currentDesktop);
        out.push([a.x, a.y, a.width, a.height]);
    }
    return out;
}
function report() {
    var list = allWindows();
    var act = active();
    var ws = [];
    for (var i = 0; i < list.length; i++) {
        var w = list[i];
        if (!w.normalWindow) continue;
        var g = w.frameGeometry;
        ws.push({
            id: String(w.internalId), x: g.x, y: g.y, w: g.width, h: g.height,
            shown: !w.minimized && onCurrentDesktop(w), full: !!w.fullScreen,
            active: w === act, pid: w.pid || 0
        });
    }
    call("Windows", JSON.stringify({ windows: ws, work_areas: workAreas() }));
}
var lastCursor = 0;
function cursor() {
    var now = Date.now();
    if (now - lastCursor < 30) return;
    lastCursor = now;
    var p = workspace.cursorPos;
    call("Cursor", Math.round(p.x), Math.round(p.y));
}
function watch(w) {
    var sigs = [w.frameGeometryChanged, w.minimizedChanged, w.fullScreenChanged,
                w.desktopsChanged, w.desktopChanged];
    for (var i = 0; i < sigs.length; i++) if (sigs[i]) sigs[i].connect(report);
}
var list = plasma6 ? workspace.windowList() : workspace.clientList();
for (var i = 0; i < list.length; i++) watch(list[i]);
var added = plasma6 ? workspace.windowAdded : workspace.clientAdded;
var removed = plasma6 ? workspace.windowRemoved : workspace.clientRemoved;
added.connect(function (w) { watch(w); report(); });
removed.connect(function () { report(); });
var more = [workspace.currentDesktopChanged, workspace.windowActivated, workspace.clientActivated,
            workspace.stackingOrderChanged, workspace.screensChanged,
            workspace.virtualScreenGeometryChanged, workspace.clientMinimized,
            workspace.clientUnminimized];
for (var i = 0; i < more.length; i++) if (more[i]) more[i].connect(report);
if (workspace.cursorPosChanged) workspace.cursorPosChanged.connect(cursor);
report();
"#;

#[derive(Debug, Deserialize)]
struct Report {
    windows: Vec<Win>,
    work_areas: Vec<[i32; 4]>,
}

#[derive(Debug, Deserialize)]
struct Win {
    id: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    shown: bool,
    full: bool,
    active: bool,
    pid: i64,
}

fn from_report(r: &Report, cursor: Option<(f32, f32)>) -> Snapshot {
    let work_areas: Vec<Rect> = r
        .work_areas
        .iter()
        .map(|&[x, y, w, h]| Rect::new(x, y, w.max(0) as u32, h.max(0) as u32))
        .collect();
    let mut s = Snapshot {
        work_areas,
        cursor,
        ..Snapshot::default()
    };
    for w in &r.windows {
        let id = id_of(&w.id);
        let rect = Rect::new(w.x, w.y, w.w.max(0) as u32, w.h.max(0) as u32);
        if w.active {
            s.active = Some(id);
            s.fullscreen = w.full;
        }
        if !w.shown {
            s.hidden.push(id);
            continue;
        }
        s.any_fullscreen |= w.full;
        // Maximized: it fills a work area (KWin scripts don't say directly).
        let maximized = s.work_areas.iter().any(|a| {
            (rect.x - a.x).abs() <= 2
                && (rect.y - a.y).abs() <= 2
                && rect.w + 4 >= a.w
                && rect.h + 4 >= a.h
        });
        s.windows.push(AppWindow {
            id,
            rect,
            collects: !w.full && !maximized,
            pid: u32::try_from(w.pid).ok().filter(|&p| p > 0),
        });
    }
    s
}

struct Bridge {
    shared: Shared,
}

#[zbus::interface(name = "org.flurry.Flurry")]
impl Bridge {
    fn windows(&self, json: String) {
        match serde_json::from_str::<Report>(&json) {
            Ok(r) => self.shared.publish(from_report(&r, self.shared.cursor())),
            Err(e) => log::debug!("kwin bridge: bad report ({e}): {json:.200}"),
        }
    }

    fn cursor(&self, x: i32, y: i32) {
        self.shared.set_cursor(Some((x as f32, y as f32)));
    }
}

/// Keeps the D-Bus service up; unloads the script when dropped.
struct Guard {
    conn: zbus::blocking::Connection,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = unload(&self.conn);
    }
}

fn scripting(
    conn: &zbus::blocking::Connection,
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
) -> zbus::Result<zbus::message::Message> {
    conn.call_method(
        Some("org.kde.KWin"),
        "/Scripting",
        Some("org.kde.kwin.Scripting"),
        method,
        body,
    )
}

fn unload(conn: &zbus::blocking::Connection) -> zbus::Result<bool> {
    scripting(conn, "unloadScript", &(PLUGIN,))?
        .body()
        .deserialize()
}

pub fn start(shared: Shared) -> Result<Option<Box<dyn Send>>> {
    let conn = zbus::blocking::connection::Builder::session()?
        .name(SERVICE)?
        .serve_at(PATH, Bridge { shared })?
        .build()
        .context("cannot serve org.flurry.Flurry on the session bus")?;
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
    let path = dir.join("flurry-kwin.js");
    std::fs::write(&path, SCRIPT).with_context(|| format!("write {}", path.display()))?;
    // A script left over from a crashed run would report twice.
    let _ = unload(&conn);
    let id: i32 = scripting(
        &conn,
        "loadScript",
        &(path.to_string_lossy().as_ref(), PLUGIN),
    )
    .context("KWin scripting not available")?
    .body()
    .deserialize()?;
    anyhow::ensure!(id >= 0, "KWin refused the script");
    scripting(&conn, "start", &())?;
    log::debug!("kwin bridge script loaded (id {id})");
    Ok(Some(Box::new(Guard { conn })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_report() {
        let r: Report = serde_json::from_str(
            r#"{"windows":[
                {"id":"{aaaa-1}","x":0,"y":0,"w":1920,"h":1036,"shown":true,"full":false,"active":false,"pid":5},
                {"id":"{aaaa-2}","x":300,"y":200,"w":800,"h":600,"shown":true,"full":false,"active":true,"pid":6},
                {"id":"{aaaa-3}","x":300,"y":200,"w":800,"h":600,"shown":false,"full":false,"active":false,"pid":7}],
                "work_areas":[[0,0,1920,1036]]}"#,
        )
        .unwrap();
        let s = from_report(&r, Some((5.0, 6.0)));
        assert_eq!(s.windows.len(), 2);
        assert!(!s.windows[0].collects, "maximized: fills the work area");
        assert!(s.windows[1].collects);
        assert_eq!(s.active, Some(id_of("{aaaa-2}")));
        assert_eq!(s.hidden, vec![id_of("{aaaa-3}")]);
        assert_eq!(s.work_areas, vec![Rect::new(0, 0, 1920, 1036)]);
        assert_eq!(s.cursor, Some((5.0, 6.0)));
    }
}
