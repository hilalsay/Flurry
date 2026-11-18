//! How hard each window's program is working: CPU use of the window's
//! process and all its descendants (so a terminal running a compiler counts
//! as hot), from `/proc/<pid>/stat`, sampled about once a second.

use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// CPU time of one process, in clock ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcTime {
    ppid: u32,
    ticks: u64,
}

/// Parse `/proc/<pid>/stat`: `pid (comm) state ppid ... utime stime ...`.
/// `comm` may contain spaces and parentheses, so split after the last `)`.
fn parse_stat(line: &str) -> Option<(u32, ProcTime)> {
    let pid = line.split_whitespace().next()?.parse().ok()?;
    let rest = &line[line.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After the comm: state(0) ppid(1) ... utime(11) stime(12).
    let ppid = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((
        pid,
        ProcTime {
            ppid,
            ticks: utime + stime,
        },
    ))
}

fn read_all(proc_dir: &Path) -> HashMap<u32, ProcTime> {
    let Ok(entries) = std::fs::read_dir(proc_dir) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
        .filter_map(|line| parse_stat(&line))
        .collect()
}

/// Samples process CPU times and reports per-window load.
pub struct Heat {
    proc_dir: std::path::PathBuf,
    ticks_per_sec: f32,
    last: Option<(Instant, HashMap<u32, ProcTime>)>,
    /// CPU use per process over the last interval (1.0 = one core).
    load: HashMap<u32, f32>,
    children: HashMap<u32, Vec<u32>>,
}

impl Heat {
    pub fn new() -> Self {
        Self::with_proc(Path::new("/proc"))
    }

    fn with_proc(proc_dir: &Path) -> Self {
        // SC_CLK_TCK is 100 on every Linux architecture flurry targets.
        Self {
            proc_dir: proc_dir.to_owned(),
            ticks_per_sec: 100.0,
            last: None,
            load: HashMap::new(),
            children: HashMap::new(),
        }
    }

    /// Take a new sample (call about once a second).
    pub fn sample(&mut self) {
        let now = Instant::now();
        let current = read_all(&self.proc_dir);
        if let Some((then, prev)) = &self.last {
            let secs = (now - *then).as_secs_f32().max(0.05);
            self.load = current
                .iter()
                .map(|(pid, t)| {
                    let before = prev.get(pid).map_or(t.ticks, |p| p.ticks.min(t.ticks));
                    (*pid, (t.ticks - before) as f32 / self.ticks_per_sec / secs)
                })
                .collect();
        }
        self.children.clear();
        for (pid, t) in &current {
            self.children.entry(t.ppid).or_default().push(*pid);
        }
        self.last = Some((now, current));
    }

    /// CPU use of `pid` and all its descendants (1.0 = one full core).
    pub fn of(&self, pid: u32) -> f32 {
        let mut total = 0.0;
        let mut stack = vec![pid];
        let mut seen = 0;
        while let Some(p) = stack.pop() {
            seen += 1;
            if seen > 10_000 {
                break; // a cycle would be a kernel bug, but don't hang on it
            }
            total += self.load.get(&p).copied().unwrap_or(0.0);
            if let Some(kids) = self.children.get(&p) {
                stack.extend(kids);
            }
        }
        total
    }
}

impl Default for Heat {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_odd_names() {
        let line =
            "1234 (Web (Content) x) S 1000 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 1";
        let (pid, t) = parse_stat(line).unwrap();
        assert_eq!(pid, 1234);
        assert_eq!(t.ppid, 1000);
        assert_eq!(t.ticks, 300);
    }

    #[test]
    fn load_includes_child_processes() {
        let dir = std::env::temp_dir().join(format!("flurry-proc-{}", std::process::id()));
        let write = |pid: u32, ppid: u32, ticks: u64| {
            let d = dir.join(pid.to_string());
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("stat"),
                format!("{pid} (p) S {ppid} 0 0 0 0 0 0 0 0 0 {ticks} 0 0 0"),
            )
            .unwrap();
        };
        // terminal(10) -> shell(11) -> compiler(12)
        write(10, 1, 0);
        write(11, 10, 0);
        write(12, 11, 0);
        let mut heat = Heat::with_proc(&dir);
        heat.sample();
        std::thread::sleep(std::time::Duration::from_millis(100));
        write(12, 11, 20); // 0.2 s of CPU in ~0.1 s: about two cores
        heat.sample();
        assert!(
            heat.of(10) > 1.0,
            "terminal is hot through its children: {}",
            heat.of(10)
        );
        assert!(heat.of(11) > 1.0);
        assert_eq!(heat.of(1), heat.of(10), "init sees everything below it");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
