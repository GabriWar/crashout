mod coredump;
mod daemon;
mod logs_browser;
mod logview;
mod tray;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Parser)]
#[command(name = "crashout", version, about = "systemd-coredump watcher + TUI")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Browse coredumps in a TUI (default).
    Tui,
    /// Follow the journal and react to every new crash.
    ///
    /// Always logs one line per crash to stderr. Desktop notifications are on
    /// by default; pass --no-notify to disable them (useful on headless boxes
    /// or when piping the log into something else).
    Watch {
        /// Don't send desktop notifications, just log to stderr.
        #[arg(long)]
        no_notify: bool,
        /// Show a systray icon (SNI). Left-click opens the TUI, right-click
        /// toggles notifications.
        #[arg(long)]
        tray: bool,
    },
    /// Print the current coredump list as JSON.
    List,
    /// Open a log file in a scrollable viewer with level colorcoding.
    Log {
        /// Path to the log file.
        file: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Tui) {
        Cmd::Tui => tui::run(),
        Cmd::Watch { no_notify, tray } => {
            let notify_enabled = Arc::new(AtomicBool::new(!no_notify));
            if tray {
                crate::tray::spawn(Arc::clone(&notify_enabled));
            }
            daemon::run(notify_enabled)
        }
        Cmd::List => {
            let dumps = coredump::list(None)?;
            println!("{}", serde_json::to_string_pretty(&dumps)?);
            Ok(())
        }
        Cmd::Log { file } => logview::run(file),
    }
}
