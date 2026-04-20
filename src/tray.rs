use anyhow::{Result, anyhow};
use ksni::{MenuItem, Tray, TrayService};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct CrashoutTray {
    pub notify_enabled: Arc<AtomicBool>,
}

impl Tray for CrashoutTray {
    fn id(&self) -> String {
        "crashout".into()
    }

    fn title(&self) -> String {
        "crashout".into()
    }

    fn icon_name(&self) -> String {
        "utilities-system-monitor".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let on = self.notify_enabled.load(Ordering::Relaxed);
        ksni::ToolTip {
            title: "crashout".into(),
            description: format!(
                "watching coredumps \u{2022} notifications {}",
                if on { "on" } else { "off" }
            ),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if let Err(e) = spawn_tui() {
            eprintln!("crashout: open tui failed: {e}");
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        use ksni::menu::*;
        let checked = self.notify_enabled.load(Ordering::Relaxed);
        vec![
            StandardItem {
                label: "Open crashout".into(),
                icon_name: "utilities-terminal".into(),
                activate: Box::new(|_: &mut Self| {
                    if let Err(e) = spawn_tui() {
                        eprintln!("crashout: open tui failed: {e}");
                    }
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Notifications".into(),
                checked,
                activate: Box::new(|t: &mut Self| {
                    let cur = t.notify_enabled.load(Ordering::Relaxed);
                    t.notify_enabled.store(!cur, Ordering::Relaxed);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_: &mut Self| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Spawn the tray service in a background thread. Returns when the service is
/// registered on the DBus. The thread keeps running for the life of the process.
pub fn spawn(notify_enabled: Arc<AtomicBool>) {
    let tray = CrashoutTray { notify_enabled };
    let service = TrayService::new(tray);
    service.spawn();
}

fn spawn_tui() -> Result<()> {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_else(|| "crashout".to_owned());

    if let Ok(t) = std::env::var("TERMINAL") {
        let trimmed = t.trim();
        if !trimmed.is_empty() {
            let mut parts = trimmed.split_whitespace();
            if let Some(prog) = parts.next() {
                // Strip single-instance flags: they break "-e <cmd>" handoff
                // because the server instance re-parses argv and loses the
                // command boundary.
                let extra: Vec<&str> = parts
                    .filter(|a| !a.to_ascii_lowercase().contains("instance"))
                    .collect();
                let flag = terminal_exec_flag(prog);
                let mut args: Vec<&str> = extra;
                args.push(flag);
                return spawn_in(prog, &args, &exe);
            }
        }
    }

    if which("xdg-terminal-exec") {
        return Command::new("xdg-terminal-exec")
            .arg(&exe)
            .arg("tui")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(Into::into);
    }

    let candidates: [(&str, &[&str]); 7] = [
        ("kitty", &["-e"]),
        ("foot", &["-e"]),
        ("alacritty", &["-e"]),
        ("wezterm", &["-e"]),
        ("konsole", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("xterm", &["-e"]),
    ];
    for (term, flag) in candidates {
        if which(term) {
            return spawn_in(term, flag, &exe);
        }
    }
    Err(anyhow!(
        "no terminal found (set $TERMINAL or install kitty/foot/alacritty/wezterm/konsole/gnome-terminal/xterm)"
    ))
}

fn spawn_in(term: &str, flags: &[&str], exe: &str) -> Result<()> {
    Command::new(term)
        .args(flags)
        .arg(exe)
        .arg("tui")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(Into::into)
}

fn terminal_exec_flag(prog: &str) -> &'static str {
    let name = prog.rsplit('/').next().unwrap_or(prog);
    match name {
        "gnome-terminal" | "tilix" | "ptyxis" => "--",
        _ => "-e",
    }
}

fn which(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
