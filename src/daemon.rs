use anyhow::{Context, Result};
use notify_rust::{Notification, Urgency};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// systemd-coredump journal MESSAGE_ID.
const COREDUMP_MESSAGE_ID: &str = "fc2e22bc6ee647b6b90729ab34a250b1";

#[derive(Debug, Deserialize)]
struct Entry {
    #[serde(rename = "COREDUMP_EXE")]
    exe: Option<String>,
    #[serde(rename = "COREDUMP_CMDLINE")]
    cmdline: Option<String>,
    #[serde(rename = "COREDUMP_PID")]
    pid: Option<String>,
    #[serde(rename = "COREDUMP_SIGNAL_NAME")]
    signal_name: Option<String>,
    #[serde(rename = "COREDUMP_SIGNAL")]
    signal: Option<String>,
    #[serde(rename = "COREDUMP_UNIT")]
    unit: Option<String>,
}

pub fn run(notify_enabled: Arc<AtomicBool>) -> Result<()> {
    let mut child = Command::new("journalctl")
        .args([
            "-f",
            "-o",
            "json",
            "--output-fields=COREDUMP_EXE,COREDUMP_CMDLINE,COREDUMP_PID,COREDUMP_SIGNAL,COREDUMP_SIGNAL_NAME,COREDUMP_UNIT",
            "-n",
            "0",
            &format!("MESSAGE_ID={COREDUMP_MESSAGE_ID}"),
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn journalctl")?;

    let stdout = child
        .stdout
        .take()
        .context("journalctl has no stdout")?;
    let reader = BufReader::new(stdout);

    eprintln!(
        "crashout: watching for coredumps (notify={})",
        if notify_enabled.load(Ordering::Relaxed) { "on" } else { "off" }
    );

    for line in reader.lines() {
        let line = line.context("reading journalctl stdout")?;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(&line) {
            Ok(entry) => {
                log_crash(&entry);
                if notify_enabled.load(Ordering::Relaxed) {
                    notify(&entry);
                }
            }
            Err(e) => eprintln!("crashout: parse error: {e}"),
        }
    }

    let status = child.wait()?;
    anyhow::bail!("journalctl exited: {status}");
}

fn log_crash(e: &Entry) {
    let exe = e.exe.as_deref().unwrap_or("?");
    let sig = e
        .signal_name
        .as_deref()
        .or(e.signal.as_deref())
        .unwrap_or("?");
    let pid = e.pid.as_deref().unwrap_or("?");
    let unit = e.unit.as_deref().unwrap_or("-");
    eprintln!("crashout: crash exe={exe} pid={pid} sig={sig} unit={unit}");
}

fn notify(e: &Entry) {
    let exe = e.exe.as_deref().unwrap_or("?");
    let name = exe.rsplit('/').next().unwrap_or(exe);
    let sig = e
        .signal_name
        .as_deref()
        .or(e.signal.as_deref())
        .unwrap_or("?");
    let pid = e.pid.as_deref().unwrap_or("?");

    let mut body = format!("pid {pid} \u{2022} {sig}\n{exe}");
    if let Some(cmd) = &e.cmdline {
        body.push('\n');
        body.push_str(cmd);
    }
    if let Some(unit) = &e.unit {
        body.push_str("\nunit: ");
        body.push_str(unit);
    }

    let _ = Notification::new()
        .summary(&format!("crash: {name}"))
        .body(&body)
        .icon("dialog-error")
        .urgency(Urgency::Critical)
        .appname("crashout")
        .show();
}
