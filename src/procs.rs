// /proc reader. zero deps, zero subprocess for the listing; per-process detail
// (maps/fds/etc.) is read on selection only. The optional `stream` tab spawns
// strace on demand and kills it as soon as you leave.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::thread;

const PAGE_KB: u64 = 4; // assume 4K pages; off by negligible amounts on others.

#[derive(Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub state: char,
    pub rss_kb: u64,
    pub threads: u32,
    pub cpu_ticks: u64, // utime + stime, accumulated
    pub cmdline: String,
}

impl ProcInfo {
    pub fn cpu_time_str(&self) -> String {
        // Assume CLK_TCK=100 (kernel default on x86/arm linux). Off platforms
        // with weird HZ get a slightly skewed display; not worth a libc dep.
        let secs = self.cpu_ticks / 100;
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{h}:{m:02}:{s:02}")
        } else {
            format!("{m}:{s:02}")
        }
    }

    pub fn rss_human(&self) -> String {
        human_kb(self.rss_kb)
    }

    pub fn display_name(&self) -> &str {
        if self.cmdline.is_empty() {
            &self.comm
        } else {
            &self.cmdline
        }
    }
}

pub fn list() -> Result<Vec<ProcInfo>> {
    let mut out = Vec::new();
    for entry in fs::read_dir("/proc")?.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else { continue };
        let Ok(pid) = s.parse::<u32>() else { continue };
        if let Some(p) = read_proc(pid) {
            out.push(p);
        }
    }
    Ok(out)
}

fn read_proc(pid: u32) -> Option<ProcInfo> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm is in parens and may contain spaces+parens; split on last ')'
    let close = stat.rfind(')')?;
    let (head, tail) = stat.split_at(close);
    let open = head.find('(')?;
    let comm = head[open + 1..].to_owned();
    let fields: Vec<&str> = tail[1..].split_whitespace().collect();
    // After comm, field index 0 = state, 1 = ppid, ... matching `man proc` minus 2.
    if fields.len() < 22 {
        return None;
    }
    let state = fields.first().and_then(|s| s.chars().next()).unwrap_or('?');
    let ppid: u32 = fields.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let utime: u64 = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    let threads: u32 = fields.get(17).and_then(|s| s.parse().ok()).unwrap_or(1);

    // statm: size resident shared text lib data dt
    let rss_kb = match fs::read_to_string(format!("/proc/{pid}/statm")) {
        Ok(s) => {
            let mut it = s.split_whitespace();
            let _vsz: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let rss_pages: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            rss_pages * PAGE_KB
        }
        Err(_) => 0,
    };

    let cmdline = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|raw| {
            let mut s: String = raw
                .into_iter()
                .map(|b| if b == 0 { ' ' } else { b as char })
                .collect();
            let trimmed = s.trim_end().to_owned();
            s.clear();
            s.push_str(&trimmed);
            s
        })
        .unwrap_or_default();

    Some(ProcInfo {
        pid,
        ppid,
        comm,
        state,
        rss_kb,
        threads,
        cpu_ticks: utime + stime,
        cmdline,
    })
}

// ---------- per-process detail ----------

pub fn status(pid: u32) -> String {
    read_or_err(format!("/proc/{pid}/status"))
}

pub fn maps(pid: u32) -> String {
    read_or_err(format!("/proc/{pid}/maps"))
}

pub fn limits(pid: u32) -> String {
    read_or_err(format!("/proc/{pid}/limits"))
}

pub fn environ(pid: u32) -> String {
    match fs::read(format!("/proc/{pid}/environ")) {
        Ok(raw) => raw
            .into_iter()
            .map(|b| if b == 0 { '\n' } else { b as char })
            .collect(),
        Err(e) => format!("(can't read: {e})"),
    }
}

pub fn fds(pid: u32) -> String {
    let dir = format!("/proc/{pid}/fd");
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => return format!("(can't read {dir}: {e})"),
    };
    let mut rows: Vec<(u32, String)> = Vec::new();
    for ent in entries.flatten() {
        let Some(name) = ent.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(num) = name.parse::<u32>() else { continue };
        let target = fs::read_link(ent.path())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|e| format!("(err: {e})"));
        rows.push((num, target));
    }
    rows.sort_by_key(|(n, _)| *n);
    let mut out = String::new();
    for (n, t) in rows {
        out.push_str(&format!("{n:>4}  {t}\n"));
    }
    if out.is_empty() {
        "(no fds visible — root may be required)".into()
    } else {
        out
    }
}

fn read_or_err(path: String) -> String {
    fs::read_to_string(&path).unwrap_or_else(|e| format!("(can't read {path}: {e})"))
}

// ---------- per-pid history (for sparklines) ----------

use std::collections::HashMap;
use std::time::Instant;

const HIST_LEN: usize = 60;

#[derive(Clone)]
pub struct Sample {
    pub cpu_pct: u64, // 0..100, integer for sparkline ergonomics
    pub rss_kb: u64,
}

pub struct History {
    rings: HashMap<u32, VecDeque<Sample>>,
    last_ticks: HashMap<u32, (u64, Instant)>,
}

impl History {
    pub fn new() -> Self {
        Self {
            rings: HashMap::new(),
            last_ticks: HashMap::new(),
        }
    }

    pub fn record(&mut self, procs: &[ProcInfo]) {
        let now = Instant::now();
        let live: std::collections::HashSet<u32> = procs.iter().map(|p| p.pid).collect();
        // Drop history for processes that vanished — keeps memory bounded.
        self.rings.retain(|pid, _| live.contains(pid));
        self.last_ticks.retain(|pid, _| live.contains(pid));

        for p in procs {
            let cpu_pct = match self.last_ticks.get(&p.pid) {
                Some((prev_ticks, prev_t)) => {
                    let elapsed = now.duration_since(*prev_t).as_millis().max(1) as u64;
                    let dt_ticks = p.cpu_ticks.saturating_sub(*prev_ticks);
                    // ticks/sec assumed 100. cpu% = ticks * 10 / ms
                    ((dt_ticks * 1000 * 10) / (elapsed * 100)).min(100)
                }
                None => 0,
            };
            self.last_ticks.insert(p.pid, (p.cpu_ticks, now));
            let ring = self.rings.entry(p.pid).or_insert_with(|| {
                VecDeque::with_capacity(HIST_LEN)
            });
            if ring.len() == HIST_LEN {
                ring.pop_front();
            }
            ring.push_back(Sample {
                cpu_pct,
                rss_kb: p.rss_kb,
            });
        }
    }

    pub fn cpu_series(&self, pid: u32) -> Vec<u64> {
        self.rings
            .get(&pid)
            .map(|r| r.iter().map(|s| s.cpu_pct).collect())
            .unwrap_or_default()
    }

    pub fn rss_series(&self, pid: u32) -> Vec<u64> {
        self.rings
            .get(&pid)
            .map(|r| r.iter().map(|s| s.rss_kb).collect())
            .unwrap_or_default()
    }

    pub fn last_cpu(&self, pid: u32) -> u64 {
        self.rings
            .get(&pid)
            .and_then(|r| r.back().map(|s| s.cpu_pct))
            .unwrap_or(0)
    }
}

// ---------- strace stream ----------

const STREAM_RING: usize = 1000;

pub struct Stream {
    pub pid: u32,
    pub lines: VecDeque<String>,
    rx: Receiver<String>,
    child: Child,
}

impl Stream {
    pub fn start(pid: u32) -> Result<Self> {
        let mut child = Command::new("strace")
            .args([
                "-p",
                &pid.to_string(),
                "-f",
                "-e",
                "trace=write",
                "-s",
                "256",
                "-q",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn strace (install strace; needs ptrace permission or root)")?;
        let stderr = child.stderr.take().context("strace stderr unavailable")?;
        let (tx, rx) = channel();
        thread::spawn(move || {
            let r = BufReader::new(stderr);
            for line in r.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            pid,
            lines: VecDeque::with_capacity(STREAM_RING),
            rx,
            child,
        })
    }

    pub fn drain(&mut self) {
        while let Ok(line) = self.rx.try_recv() {
            if self.lines.len() == STREAM_RING {
                self.lines.pop_front();
            }
            self.lines.push_back(line);
        }
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        for l in &self.lines {
            s.push_str(l);
            s.push('\n');
        }
        s
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------- formatting ----------

fn human_kb(kb: u64) -> String {
    const K: f64 = 1024.0;
    let v = kb as f64;
    if v < K {
        format!("{kb}K")
    } else if v < K * K {
        format!("{:.1}M", v / K)
    } else {
        format!("{:.1}G", v / (K * K))
    }
}
