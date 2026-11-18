//! PLAN.md Phase 2 automated checks, against a private headless Xvfb server:
//! - without a compositor, flurry exits with the compositor error;
//! - with --no-compositor-check it runs 5 s without crashing, and its
//!   window is a click-through, override-redirect, 32-bit overlay.
//!
//! Skipped (with a message) when `Xvfb` is not installed. Set
//! `FLURRY_TEST_XSERVER=Xephyr` to use a (visible) nested Xephyr instead.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as _, SK};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};

struct Xvfb {
    child: Child,
    display: String,
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The X server binary and its arguments for a 1280×720 screen.
fn server() -> (String, Vec<&'static str>) {
    match std::env::var("FLURRY_TEST_XSERVER").as_deref() {
        Ok("Xephyr") => ("Xephyr".into(), vec!["-screen", "1280x720"]),
        _ => ("Xvfb".into(), vec!["-screen", "0", "1280x720x24"]),
    }
}

fn start_xvfb() -> Option<Xvfb> {
    let (bin, args) = server();
    let found = Command::new(&bin)
        .arg("-help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok();
    if !found {
        eprintln!("SKIPPED: {bin} not installed (sudo apt install xvfb)");
        return None;
    }
    let base = 90 + (std::process::id() % 100) as u16;
    for n in base..base + 50 {
        if Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
            || Path::new(&format!("/tmp/.X{n}-lock")).exists()
        {
            continue;
        }
        let display = format!(":{n}");
        let child = Command::new(&bin)
            .arg(&display)
            .args(&args)
            .args(["-nolisten", "tcp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn X server");
        let xvfb = Xvfb { child, display };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if x11rb::connect(Some(&xvfb.display)).is_ok() {
                return Some(xvfb);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("{bin} on {} did not start", xvfb.display);
    }
    panic!("no free X display number");
}

fn flurry(display: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_flurry"));
    cmd.env("DISPLAY", display)
        .env_remove("WAYLAND_DISPLAY")
        .env("RUST_LOG", "debug")
        .env("XDG_CONFIG_HOME", "/nonexistent") // defaults, not the user's config
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Finds windows with WM_CLASS "flurry" among the root's children.
fn flurry_windows(display: &str) -> Vec<u32> {
    let (conn, screen) = x11rb::connect(Some(display)).expect("connect");
    let root = conn.setup().roots[screen].root;
    let tree = conn.query_tree(root).unwrap().reply().unwrap();
    tree.children
        .into_iter()
        .filter(|&w| {
            conn.get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64)
                .unwrap()
                .reply()
                .is_ok_and(|p| p.value.starts_with(b"flurry\0"))
        })
        .collect()
}

fn check_overlay_window(display: &str) {
    let windows = flurry_windows(display);
    assert_eq!(
        windows.len(),
        1,
        "expected one overlay window on a 1-monitor screen"
    );
    let (conn, _) = x11rb::connect(Some(display)).expect("connect");
    let w = windows[0];
    let attrs = conn.get_window_attributes(w).unwrap().reply().unwrap();
    assert!(attrs.override_redirect, "overlay must be override-redirect");
    assert_eq!(attrs.map_state, x11rb::protocol::xproto::MapState::VIEWABLE);
    let geom = conn.get_geometry(w).unwrap().reply().unwrap();
    assert_eq!(geom.depth, 32, "overlay must use a 32-bit ARGB visual");
    assert_eq!((geom.width, geom.height), (1280, 720));
    let input = conn
        .shape_get_rectangles(w, SK::INPUT)
        .unwrap()
        .reply()
        .unwrap();
    assert!(
        input.rectangles.is_empty(),
        "input shape must be empty (click-through)"
    );
}

/// Tests start their own X server; run them one at a time so they don't
/// race for display numbers.
static X_SERVER: Mutex<()> = Mutex::new(());

#[test]
fn x11_overlay_under_xvfb() {
    let _lock = X_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(xvfb) = start_xvfb() else { return };

    // 1. Xvfb runs no compositor: flurry must refuse with a clear message.
    let out = flurry(&xvfb.display).output().expect("run flurry");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "should fail without compositor");
    assert!(
        stderr.contains("no compositing manager") && stderr.contains("picom"),
        "unexpected stderr:\n{stderr}"
    );

    // 2. --no-compositor-check: runs for 5 s, creates a proper overlay.
    let mut child = flurry(&xvfb.display)
        .arg("--no-compositor-check")
        .spawn()
        .expect("spawn flurry");
    std::thread::sleep(Duration::from_millis(1500));
    check_overlay_window(&xvfb.display);
    std::thread::sleep(Duration::from_millis(3500));
    let still_running = child.try_wait().expect("try_wait").is_none();
    let _ = child.kill();
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(still_running, "flurry exited early:\n{stderr}");
    assert!(!stderr.contains("ERROR"), "errors logged:\n{stderr}");
    assert!(!stderr.contains("X11 error"), "X errors:\n{stderr}");
    assert!(!stderr.contains("panicked"), "panic:\n{stderr}");
}

/// Runs `flurry <args>` as a client against the daemon using `runtime_dir`.
fn client(runtime_dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_flurry"))
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .output()
        .expect("run client");
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned()
        + String::from_utf8_lossy(&out.stderr).trim();
    (out.status.success(), text)
}

fn wait_for(cond: impl Fn() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// PLAN.md Phase 3: CLI commands reach the daemon; quit and SIGTERM remove the socket.
#[test]
fn controls_over_ipc() {
    let _lock = X_SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(xvfb) = start_xvfb() else { return };
    let runtime = std::env::temp_dir().join(format!("flurry-ipc-test-{}", std::process::id()));
    std::fs::create_dir_all(&runtime).unwrap();
    let socket = runtime.join("flurry.sock");
    let start = || {
        flurry(&xvfb.display)
            .arg("--no-compositor-check")
            .env("XDG_RUNTIME_DIR", &runtime)
            .spawn()
            .expect("spawn flurry")
    };

    let (ok, text) = client(&runtime, &["status"]);
    assert!(!ok && text.contains("not running"), "{text}");

    let mut daemon = start();
    wait_for(|| socket.exists(), "socket");
    let (ok, text) = client(&runtime, &["status"]);
    assert!(
        ok && text.starts_with("running intensity=medium fps=30"),
        "{text}"
    );
    assert_eq!(client(&runtime, &["toggle"]), (true, "paused".into()));
    assert!(client(&runtime, &["status"]).1.starts_with("paused"));
    assert_eq!(client(&runtime, &["toggle"]), (true, "resumed".into()));
    assert_eq!(
        client(&runtime, &["intensity", "heavy"]),
        (true, "intensity heavy".into())
    );
    assert_eq!(client(&runtime, &["fps", "60"]), (true, "fps 60".into()));
    let (_, text) = client(&runtime, &["status"]);
    assert!(text.contains("intensity=heavy fps=60 flakes=267"), "{text}");
    let (ok, _) = client(&runtime, &["fps", "0"]);
    assert!(!ok, "fps 0 must be rejected");

    // A second daemon refuses to start while the first owns the socket.
    let second = flurry(&xvfb.display)
        .arg("--no-compositor-check")
        .env("XDG_RUNTIME_DIR", &runtime)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    assert_eq!(client(&runtime, &["quit"]), (true, "quitting".into()));
    let status = daemon.wait().unwrap();
    assert!(status.success(), "quit should exit 0: {status:?}");
    assert!(!socket.exists(), "socket must be removed on quit");

    // SIGTERM (and Ctrl+C) also shut down cleanly.
    let mut daemon = start();
    wait_for(|| socket.exists(), "socket");
    let pid = daemon.id().to_string();
    Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(daemon.wait().unwrap().success(), "SIGTERM should exit 0");
    assert!(!socket.exists(), "socket must be removed on SIGTERM");
    std::fs::remove_dir_all(&runtime).ok();
}
