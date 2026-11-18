//! Control channel: a UNIX socket at `$XDG_RUNTIME_DIR/flurry.sock`.
//!
//! The protocol is one text line per connection: the client sends a command
//! (`toggle`, `intensity heavy`, …) and the daemon answers with one line,
//! starting with `error:` on failure. The first instance owns the socket;
//! a second daemon refuses to start.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::config::Intensity;
use crate::power::PauseReason;

/// Something the daemon can be asked to do (CLI, tray, hotkey, signals).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Toggle,
    Clear,
    Intensity(Intensity),
    Fps(u32),
    Play,
    Reload,
    Status,
    Quit,
    /// Internal (not accepted over the socket): an automatic pause reason
    /// started or ended.
    Auto(PauseReason, bool),
    /// Internal: the machine woke from sleep.
    Wake,
    /// Internal: new real weather (feature 5).
    Weather(crate::weather::Effect),
    /// Internal: a bass hit in the music (feature 7), strength 0.2..=2.
    Gust(f32),
}

impl Command {
    pub fn encode(&self) -> String {
        match self {
            Command::Toggle => "toggle".into(),
            Command::Clear => "clear".into(),
            Command::Intensity(i) => format!("intensity {}", intensity_name(*i)),
            Command::Fps(n) => format!("fps {n}"),
            Command::Play => "play".into(),
            Command::Reload => "reload".into(),
            Command::Status => "status".into(),
            Command::Quit => "quit".into(),
            Command::Auto(r, on) => {
                format!("auto-pause {} {}", r.name(), if *on { "on" } else { "off" })
            }
            Command::Wake => "wake".into(),
            Command::Weather(e) => format!("weather {e:?}"),
            Command::Gust(s) => format!("gust {s:.2}"),
        }
    }

    pub fn parse(line: &str) -> Result<Command> {
        let mut words = line.split_whitespace();
        let cmd = match (words.next(), words.next()) {
            (Some("toggle"), None) => Command::Toggle,
            (Some("clear"), None) => Command::Clear,
            (Some("intensity"), Some(level)) => Command::Intensity(match level {
                "light" => Intensity::Light,
                "medium" => Intensity::Medium,
                "heavy" => Intensity::Heavy,
                _ => bail!("unknown intensity {level:?} (light, medium, heavy)"),
            }),
            (Some("fps"), Some(n)) => Command::Fps(n.parse().context("fps must be a number")?),
            (Some("play"), None) => Command::Play,
            (Some("reload"), None) => Command::Reload,
            (Some("status"), None) => Command::Status,
            (Some("quit"), None) => Command::Quit,
            _ => bail!("unknown command {line:?}"),
        };
        if words.next().is_some() {
            bail!("too many arguments in {line:?}");
        }
        Ok(cmd)
    }
}

pub fn intensity_name(i: Intensity) -> &'static str {
    match i {
        Intensity::Light => "light",
        Intensity::Medium => "medium",
        Intensity::Heavy => "heavy",
    }
}

/// A command plus, for socket clients, where to send the answer.
pub struct Request {
    pub cmd: Command,
    pub reply: Option<Sender<String>>,
}

impl Request {
    pub fn fire(cmd: Command) -> Request {
        Request { cmd, reply: None }
    }
}

/// `$XDG_RUNTIME_DIR/flurry.sock`, or a per-user path in /tmp without it.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => Path::new(&dir).join("flurry.sock"),
        None => {
            let uid = rustix::process::getuid().as_raw();
            std::env::temp_dir().join(format!("flurry-{uid}.sock"))
        }
    }
}

/// Owns the listening socket; removes the socket file when dropped.
pub struct Server {
    path: PathBuf,
}

impl Server {
    /// Bind the socket and serve requests on a background thread.
    pub fn start(path: PathBuf, tx: Sender<Request>) -> Result<Server> {
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                bail!(
                    "flurry is already running (socket {}); use `flurry quit` to stop it",
                    path.display()
                );
            }
            log::debug!("removing stale socket {}", path.display());
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("cannot create socket {}", path.display()))?;
        log::debug!("listening on {}", path.display());
        std::thread::Builder::new()
            .name("ipc".into())
            .spawn(move || serve(listener, tx))?;
        Ok(Server { path })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            log::debug!("cannot remove {}: {e}", self.path.display());
        }
    }
}

fn serve(listener: UnixListener, tx: Sender<Request>) {
    for stream in listener.incoming() {
        let result = stream
            .map_err(anyhow::Error::from)
            .and_then(|s| handle_client(s, &tx));
        if let Err(e) = result {
            log::debug!("ipc client: {e:#}");
        }
    }
}

fn handle_client(stream: UnixStream, tx: &Sender<Request>) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let answer = match Command::parse(line.trim()) {
        Ok(cmd) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(Request {
                cmd,
                reply: Some(reply_tx),
            })
            .context("daemon is shutting down")?;
            reply_rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| "error: no answer from the daemon".into())
        }
        Err(e) => format!("error: {e}"),
    };
    let mut stream = stream;
    writeln!(stream, "{answer}")?;
    Ok(())
}

/// Client side: send `cmd` to the running daemon and return its answer.
pub fn send(path: &Path, cmd: Command) -> Result<String> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "flurry is not running (no daemon on {}); start it with `flurry`",
            path.display()
        )
    })?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{}", cmd.encode())?;
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer)?;
    Ok(answer.trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip() {
        for cmd in [
            Command::Toggle,
            Command::Clear,
            Command::Intensity(Intensity::Heavy),
            Command::Fps(60),
            Command::Play,
            Command::Reload,
            Command::Status,
            Command::Quit,
        ] {
            assert_eq!(Command::parse(&cmd.encode()).unwrap(), cmd);
        }
        assert!(Command::parse("intensity extreme").is_err());
        assert!(Command::parse("toggle now").is_err());
        assert!(Command::parse("").is_err());
    }

    #[test]
    fn server_answers_and_refuses_second_instance() {
        let dir = std::env::temp_dir().join(format!("flurry-ipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("flurry.sock");
        let (tx, rx) = mpsc::channel::<Request>();
        let server = Server::start(path.clone(), tx.clone()).unwrap();
        std::thread::spawn(move || {
            for req in rx {
                if let Some(r) = req.reply {
                    let _ = r.send(format!("ok: {}", req.cmd.encode()));
                }
            }
        });
        assert_eq!(send(&path, Command::Toggle).unwrap(), "ok: toggle");
        assert!(Server::start(path.clone(), tx).is_err(), "second instance");
        drop(server);
        assert!(!path.exists(), "socket removed on drop");
        assert!(send(&path, Command::Toggle).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
