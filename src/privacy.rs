//! Smart privacy pause (feature 8): pause while the screen is being shared.
//! Two signals, either is enough:
//! - a sharing indicator window is open (title heuristics; on X11 browsers
//!   and most apps capture the screen directly, so this is what works there);
//! - PipeWire has a running video *consumer* stream (`Stream/Input/Video`),
//!   as with portal-based screen casts. Device nodes (`Video/Source`, e.g.
//!   the webcam) don't count: they always exist.

use std::process::Command as Process;
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::ipc::{Command, Request};
use crate::power::PauseReason;

/// Window titles that apps show only while they share the screen.
const INDICATOR_TITLES: [&str; 5] = [
    "is sharing your screen",
    "is sharing a window",
    "is sharing this tab",
    "is sharing your entire screen",
    "sharing indicator", // Firefox
];

/// Is `title` a screen-sharing indicator window?
pub fn is_share_indicator(title: &str) -> bool {
    let t = title.to_lowercase();
    INDICATOR_TITLES.iter().any(|p| t.contains(p))
}

/// Does `pw-dump` output show a running video consumer stream?
fn video_stream_running(pw_dump: &str) -> bool {
    let Ok(serde_json::Value::Array(objects)) = serde_json::from_str(pw_dump) else {
        return false;
    };
    objects.iter().any(|o| {
        let info = &o["info"];
        o["type"] == "PipeWire:Interface:Node"
            && info["props"]["media.class"] == "Stream/Input/Video"
            && info["state"] == "running"
    })
}

const POLL: Duration = Duration::from_secs(10);

/// Poll PipeWire every 10 s; report `ScreenShare` changes.
pub fn watch_pipewire(tx: Sender<Request>) {
    let run = move || {
        let mut last = false;
        loop {
            let out = match Process::new("pw-dump").output() {
                Ok(o) if o.status.success() => o,
                _ => {
                    log::info!("pw-dump unavailable: PipeWire screen casts aren't detected");
                    return;
                }
            };
            let sharing = video_stream_running(&String::from_utf8_lossy(&out.stdout));
            if sharing != last {
                last = sharing;
                let cmd = Command::Auto(PauseReason::ScreenShare, sharing);
                if tx.send(Request::fire(cmd)).is_err() {
                    return;
                }
            }
            std::thread::sleep(POLL);
        }
    };
    if let Err(e) = std::thread::Builder::new()
        .name("pipewire".into())
        .spawn(run)
    {
        log::warn!("cannot start the PipeWire watcher: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_titles() {
        assert!(is_share_indicator(
            "meet.google.com is sharing your screen."
        ));
        assert!(is_share_indicator("Zoom Web is sharing a window."));
        assert!(is_share_indicator("Firefox — Sharing Indicator"));
        assert!(!is_share_indicator("Screen sharing tips - Notes"));
        assert!(!is_share_indicator("Terminal"));
    }

    #[test]
    fn only_running_video_consumers_count() {
        let node = |class: &str, state: &str| {
            format!(
                r#"{{"type":"PipeWire:Interface:Node","info":{{"state":"{state}","props":{{"media.class":"{class}"}}}}}}"#
            )
        };
        let dump = |nodes: &[String]| format!("[{}]", nodes.join(","));
        assert!(
            !video_stream_running(&dump(&[node("Video/Source", "running")])),
            "webcam device"
        );
        assert!(!video_stream_running(&dump(&[node(
            "Stream/Input/Video",
            "suspended"
        )])));
        assert!(video_stream_running(&dump(&[
            node("Audio/Sink", "running"),
            node("Stream/Input/Video", "running")
        ])));
        assert!(!video_stream_running("not json"));
    }
}
