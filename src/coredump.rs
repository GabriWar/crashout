use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dump {
    pub time: u64,
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub sig: u32,
    pub corefile: String,
    pub exe: String,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Age {
    Hour,
    Day,
    Week,
    Older,
}

impl Dump {
    pub fn exe_name(&self) -> &str {
        self.exe.rsplit('/').next().unwrap_or(&self.exe)
    }

    pub fn signal_name(&self) -> &'static str {
        match self.sig {
            1 => "SIGHUP",
            2 => "SIGINT",
            3 => "SIGQUIT",
            4 => "SIGILL",
            6 => "SIGABRT",
            7 => "SIGBUS",
            8 => "SIGFPE",
            9 => "SIGKILL",
            11 => "SIGSEGV",
            13 => "SIGPIPE",
            14 => "SIGALRM",
            15 => "SIGTERM",
            24 => "SIGXCPU",
            25 => "SIGXFSZ",
            31 => "SIGSYS",
            _ => "SIG?",
        }
    }

    pub fn time_string(&self) -> String {
        let secs = (self.time / 1_000_000) as i64;
        format_local(secs)
    }

    pub fn time_secs(&self) -> i64 {
        (self.time / 1_000_000) as i64
    }

    pub fn size_human(&self) -> String {
        match self.size {
            Some(b) => human_bytes(b),
            None => String::from("-"),
        }
    }

    pub fn corefile_present(&self) -> bool {
        self.corefile == "present"
    }

    pub fn age(&self) -> Age {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let delta = now - self.time_secs();
        if delta < 3600 {
            Age::Hour
        } else if delta < 86_400 {
            Age::Day
        } else if delta < 604_800 {
            Age::Week
        } else {
            Age::Older
        }
    }
}

pub fn list(filter: Option<&str>) -> Result<Vec<Dump>> {
    let mut cmd = Command::new("coredumpctl");
    cmd.arg("list").arg("--json=short").arg("--reverse");
    if let Some(f) = filter {
        cmd.arg(f);
    }
    let out = cmd
        .stderr(Stdio::piped())
        .output()
        .context("failed to spawn coredumpctl")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("No coredumps") {
            return Ok(Vec::new());
        }
        bail!("coredumpctl failed: {stderr}");
    }
    if out.stdout.trim_ascii().is_empty() {
        return Ok(Vec::new());
    }
    let dumps: Vec<Dump> = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("parsing coredumpctl json: {}", String::from_utf8_lossy(&out.stdout)))?;
    Ok(dumps)
}

pub fn info(pid: u32) -> Result<String> {
    let out = Command::new("coredumpctl")
        .arg("info")
        .arg(pid.to_string())
        .output()
        .context("failed to spawn coredumpctl info")?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        s.push('\n');
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(s)
}

/// Fetch selected journal fields for a specific crash.
pub fn journal_fields(pid: u32, fields: &[&str]) -> Result<HashMap<String, String>> {
    let output_fields = fields.join(",");
    let out = Command::new("journalctl")
        .args([
            "MESSAGE_ID=fc2e22bc6ee647b6b90729ab34a250b1",
            &format!("COREDUMP_PID={pid}"),
            "-o",
            "json",
            "-n",
            "1",
            "--output-fields",
            &output_fields,
        ])
        .output()
        .context("failed to spawn journalctl")?;
    let mut map = HashMap::new();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some(line) = stdout.lines().next() else {
        return Ok(map);
    };
    let value: serde_json::Value = serde_json::from_str(line)?;
    if let serde_json::Value::Object(obj) = value {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                map.insert(k, s.to_owned());
            }
        }
    }
    Ok(map)
}

/// Gather journal lines related to a specific crashed process:
/// everything journald has for that pid, plus ±5min of same-COMM lines.
pub fn journal_for_dump(dump: &Dump) -> Result<String> {
    let by_pid = Command::new("journalctl")
        .args([
            &format!("_PID={}", dump.pid),
            "--no-pager",
            "-o",
            "short-iso",
        ])
        .output()
        .context("journalctl by pid")?;
    let pid_text = String::from_utf8_lossy(&by_pid.stdout).into_owned();

    let crash_ts = dump.time_secs();
    let since = crash_ts - 300;
    let until = crash_ts + 60;
    let by_comm = Command::new("journalctl")
        .args([
            &format!("_COMM={}", dump.exe_name()),
            "--since",
            &format!("@{since}"),
            "--until",
            &format!("@{until}"),
            "--no-pager",
            "-o",
            "short-iso",
        ])
        .output()
        .context("journalctl by comm")?;
    let comm_text = String::from_utf8_lossy(&by_comm.stdout).into_owned();

    let pid_clean = strip_no_entries(&pid_text);
    let comm_clean = strip_no_entries(&comm_text);

    let mut s = String::new();
    if !pid_clean.trim().is_empty() {
        s.push_str("=== by PID ===\n");
        s.push_str(&pid_clean);
        if !pid_clean.ends_with('\n') {
            s.push('\n');
        }
    }
    if !comm_clean.trim().is_empty() {
        s.push_str("\n=== by COMM \u{00B1}5min ===\n");
        s.push_str(&comm_clean);
    }
    if s.is_empty() {
        s.push_str("no journal entries found for this process (probably didn't log to journald)");
    }
    Ok(s)
}

fn strip_no_entries(s: &str) -> String {
    s.lines()
        .filter(|l| !l.starts_with("-- No entries"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run gdb in batch mode against the coredump and return the backtrace.
pub fn backtrace(dump: &Dump) -> Result<String> {
    if !dump.corefile_present() {
        bail!("corefile missing — can't backtrace");
    }
    let tmp = std::env::temp_dir().join(format!("crashout-{}.core", dump.pid));
    let dump_status = Command::new("coredumpctl")
        .args(["dump", &dump.pid.to_string(), "-o"])
        .arg(&tmp)
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .context("failed to spawn coredumpctl dump")?;
    if !dump_status.success() {
        bail!("coredumpctl dump failed");
    }
    let out = Command::new("gdb")
        .args([
            "-batch",
            "-ex",
            "set pagination off",
            "-ex",
            "thread apply all bt full",
            &dump.exe,
        ])
        .arg(&tmp)
        .output()
        .context("failed to spawn gdb")?;
    let _ = std::fs::remove_file(&tmp);
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        s.push('\n');
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(s)
}

/// Locate the corefile on disk (parsed from `coredumpctl info`) and remove it.
pub fn delete_corefile(pid: u32) -> Result<String> {
    let info_text = info(pid)?;
    let storage = info_text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Storage:").map(str::trim))
        .context("no Storage: line in coredumpctl info")?;
    let path = storage
        .split_whitespace()
        .next()
        .context("empty Storage path")?;
    if !path.starts_with('/') {
        bail!("unexpected Storage value: {storage}");
    }
    std::fs::remove_file(path).with_context(|| format!("rm {path}"))?;
    Ok(path.to_owned())
}

pub fn copy_to_clipboard(s: &str) -> Result<&'static str> {
    let candidates: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["-b", "-i"]),
    ];
    for (cmd, args) in candidates {
        if Command::new("which")
            .arg(cmd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            let mut child = Command::new(cmd)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(s.as_bytes())?;
            }
            child.wait()?;
            return Ok(match cmd {
                "wl-copy" => "wl-copy",
                "xclip" => "xclip",
                _ => "xsel",
            });
        }
    }
    bail!("no clipboard tool found (install wl-clipboard, xclip, or xsel)")
}

fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let v = b as f64;
    if v < K {
        format!("{b} B")
    } else if v < K * K {
        format!("{:.1}K", v / K)
    } else if v < K * K * K {
        format!("{:.1}M", v / (K * K))
    } else {
        format!("{:.1}G", v / (K * K * K))
    }
}

fn format_local(secs_utc: i64) -> String {
    let off = local_offset_seconds().unwrap_or(0);
    let secs = secs_utc + off;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}:{second:02}")
}

fn local_offset_seconds() -> Option<i64> {
    let out = Command::new("date").arg("+%z").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.len() < 5 {
        return None;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let hh: i64 = s[1..3].parse().ok()?;
    let mm: i64 = s[3..5].parse().ok()?;
    Some(sign * (hh * 3600 + mm * 60))
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d)
}
