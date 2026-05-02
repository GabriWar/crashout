mod coredump;
mod daemon;
mod logs_browser;
mod logview;
mod procs;
mod procs_screen;
mod tray;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Parser)]
#[command(
    name = "crashout",
    version,
    about = "process debugger: crashes \u{2022} logs \u{2022} procs"
)]
struct Cli {
    /// Skip desktop notifications when running the default watch daemon.
    #[arg(long, global = true)]
    no_notify: bool,
    /// Show a systray icon when running the default watch daemon.
    #[arg(long, global = true)]
    tray: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Open the TUI (crashes / logs / procs).
    Tui,
    /// Same as the no-arg default: follow the journal and react to every new
    /// crash. Logs one line per crash to stderr; sends desktop notifications
    /// unless --no-notify is passed.
    Watch,
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
    match cli.cmd.unwrap_or(Cmd::Watch) {
        Cmd::Watch => {
            let notify_enabled = Arc::new(AtomicBool::new(!cli.no_notify));
            if cli.tray {
                crate::tray::spawn(Arc::clone(&notify_enabled));
            }
            daemon::run(notify_enabled)
        }
        Cmd::Tui => tui::run(),
        Cmd::List => {
            let dumps = coredump::list(None)?;
            println!("{}", serde_json::to_string_pretty(&dumps)?);
            Ok(())
        }
        Cmd::Log { file } => logview::run(file),
    }
}
