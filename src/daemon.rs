use anyhow::{Context, Result};
use notify_rust::{Notification, Urgency};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use systemd::journal::{Journal, JournalFiles, JournalSeek, JournalWaitResult};

/// systemd-coredump journal MESSAGE_ID.
const COREDUMP_MESSAGE_ID: &str = "fc2e22bc6ee647b6b90729ab34a250b1";

struct Entry {
    exe: Option<String>,
    cmdline: Option<String>,
    pid: Option<String>,
    signal_name: Option<String>,
    signal: Option<String>,
    unit: Option<String>,
}

impl Entry {
    fn from_record(r: &BTreeMap<String, String>) -> Self {
        Self {
            exe: r.get("COREDUMP_EXE").cloned(),
            cmdline: r.get("COREDUMP_CMDLINE").cloned(),
            pid: r.get("COREDUMP_PID").cloned(),
            signal_name: r.get("COREDUMP_SIGNAL_NAME").cloned(),
            signal: r.get("COREDUMP_SIGNAL").cloned(),
            unit: r.get("COREDUMP_UNIT").cloned(),
        }
    }
}

pub fn run(notify_enabled: Arc<AtomicBool>) -> Result<()> {
    let mut journal = Journal::open(JournalFiles::All, false, false)
        .context("open journal")?;
    journal
        .match_add("MESSAGE_ID", COREDUMP_MESSAGE_ID)
        .context("add match")?;
    // Position past the tail so only *new* entries fire.
    journal.seek(JournalSeek::Tail).context("seek tail")?;

    eprintln!(
        "crashout: watching for coredumps (notify={})",
        if notify_enabled.load(Ordering::Relaxed) { "on" } else { "off" }
    );

    loop {
        while let Some(record) = journal.next_entry().context("next entry")? {
            let entry = Entry::from_record(&record);
            log_crash(&entry);
            if notify_enabled.load(Ordering::Relaxed) {
                notify(&entry);
            }
        }
        // Block up to 10 minutes — then loop to let thread react to signals.
        let _: JournalWaitResult = journal
            .wait(Some(Duration::from_secs(600)))
            .context("journal wait")?;
    }
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
