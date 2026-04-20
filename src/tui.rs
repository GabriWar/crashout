use crate::coredump::{self, Age, Dump};
use crate::logs_browser::LogsBrowser;
use crate::logview;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = App::new()?.run(&mut terminal);
    ratatui::restore();
    result
}

// ---------- state ----------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Tab {
    Info,
    Backtrace,
    Logs,
    Env,
    Cmdline,
}
impl Tab {
    const ORDER: [Tab; 5] = [Tab::Info, Tab::Backtrace, Tab::Logs, Tab::Env, Tab::Cmdline];
    fn label(self) -> &'static str {
        match self {
            Tab::Info => "info",
            Tab::Backtrace => "backtrace",
            Tab::Logs => "logs",
            Tab::Env => "env",
            Tab::Cmdline => "cmdline",
        }
    }
    fn next(self, step: i32) -> Tab {
        let idx = Self::ORDER.iter().position(|t| *t == self).unwrap_or(0) as i32;
        let n = Self::ORDER.len() as i32;
        let i = ((idx + step).rem_euclid(n)) as usize;
        Self::ORDER[i]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortMode {
    TimeDesc,
    TimeAsc,
    Exe,
    Sig,
    SizeDesc,
}
impl SortMode {
    fn next(self) -> SortMode {
        match self {
            Self::TimeDesc => Self::TimeAsc,
            Self::TimeAsc => Self::Exe,
            Self::Exe => Self::Sig,
            Self::Sig => Self::SizeDesc,
            Self::SizeDesc => Self::TimeDesc,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::TimeDesc => "time\u{2193}",
            Self::TimeAsc => "time\u{2191}",
            Self::Exe => "exe",
            Self::Sig => "sig",
            Self::SizeDesc => "size\u{2193}",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SinceFilter {
    All,
    Hour,
    Day,
    Week,
    Boot,
}
impl SinceFilter {
    fn next(self) -> SinceFilter {
        match self {
            Self::All => Self::Hour,
            Self::Hour => Self::Day,
            Self::Day => Self::Week,
            Self::Week => Self::Boot,
            Self::Boot => Self::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Hour => "1h",
            Self::Day => "1d",
            Self::Week => "1w",
            Self::Boot => "boot",
        }
    }
}

const SIG_FILTER_CYCLE: [Option<u32>; 8] = [
    None,
    Some(11),
    Some(6),
    Some(7),
    Some(4),
    Some(8),
    Some(9),
    Some(15),
];

fn sig_label(s: Option<u32>) -> &'static str {
    match s {
        None => "all",
        Some(4) => "SIGILL",
        Some(6) => "SIGABRT",
        Some(7) => "SIGBUS",
        Some(8) => "SIGFPE",
        Some(9) => "SIGKILL",
        Some(11) => "SIGSEGV",
        Some(15) => "SIGTERM",
        _ => "?",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Filter,
    Yank,
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Crashes,
    Logs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CrashView {
    List,
    Detail,
}

struct App {
    all: Vec<Dump>,
    visible: Vec<Dump>,
    counts: Vec<usize>,
    list_state: ListState,

    tab: Tab,
    preview_scroll: u16,
    preview_cache: HashMap<(u32, Tab), String>,
    bt_rx: Option<Receiver<(u32, Result<String, String>)>>,
    bt_in_flight: Option<u32>,

    exe_filter: String,
    sig_filter: Option<u32>,
    since_filter: SinceFilter,
    only_failed_unit: bool,
    grouped: bool,
    sort_mode: SortMode,

    mode: Mode,
    seen_pids: HashSet<u32>,
    new_pids: HashSet<u32>,
    last_poll: Instant,
    boot_time_us: u64,

    status: String,

    screen: Screen,
    crash_view: CrashView,
    logs_browser: Option<LogsBrowser>,
}

impl App {
    fn new() -> Result<Self> {
        let all = coredump::list(None)?;
        let mut app = Self {
            all,
            visible: Vec::new(),
            counts: Vec::new(),
            list_state: ListState::default(),
            tab: Tab::Info,
            preview_scroll: 0,
            preview_cache: HashMap::new(),
            bt_rx: None,
            bt_in_flight: None,
            exe_filter: String::new(),
            sig_filter: None,
            since_filter: SinceFilter::All,
            only_failed_unit: false,
            grouped: false,
            sort_mode: SortMode::TimeDesc,
            mode: Mode::Normal,
            seen_pids: HashSet::new(),
            new_pids: HashSet::new(),
            last_poll: Instant::now(),
            boot_time_us: boot_time_us(),
            status: String::from("? for help"),
            screen: Screen::Crashes,
            crash_view: CrashView::List,
            logs_browser: None,
        };
        app.seen_pids = app.all.iter().map(|d| d.pid).collect();
        app.rebuild_view();
        Ok(app)
    }

    // ---------- main loop ----------

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            self.poll_background();
            terminal.draw(|f| self.draw(f))?;

            if self.last_poll.elapsed() >= Duration::from_secs(2) && self.screen == Screen::Crashes {
                self.auto_reload();
            }

            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if self.mode == Mode::Help {
                self.mode = Mode::Normal;
                continue;
            }

            let logs_filtering = self
                .logs_browser
                .as_ref()
                .map(|b| b.is_filtering())
                .unwrap_or(false);
            let text_input_active =
                self.mode == Mode::Filter || (self.screen == Screen::Logs && logs_filtering);

            if !text_input_active {
                match key.code {
                    KeyCode::Char('1') => {
                        self.screen = Screen::Crashes;
                        continue;
                    }
                    KeyCode::Char('2') => {
                        if self.logs_browser.is_none() {
                            self.logs_browser = Some(LogsBrowser::new());
                        }
                        self.screen = Screen::Logs;
                        continue;
                    }
                    KeyCode::Char('?') => {
                        self.mode = Mode::Help;
                        continue;
                    }
                    _ => {}
                }
            }

            match self.screen {
                Screen::Crashes => match self.mode {
                    Mode::Help => {}
                    Mode::Filter => self.handle_filter_key(key.code),
                    Mode::Yank => {
                        self.handle_yank_key(key.code);
                        self.mode = Mode::Normal;
                    }
                    Mode::Normal => {
                        if self.handle_normal_key(key.code, key.modifiers, terminal)? {
                            return Ok(());
                        }
                    }
                },
                Screen::Logs => {
                    let lb_mode = self
                        .logs_browser
                        .as_ref()
                        .map(|b| b.mode())
                        .unwrap_or(crate::logs_browser::LBMode::Browser);
                    if !logs_filtering {
                        match (key.code, key.modifiers) {
                            (KeyCode::Char('q'), _) => return Ok(()),
                            (KeyCode::Esc, _) => {
                                if lb_mode == crate::logs_browser::LBMode::Browser {
                                    return Ok(());
                                }
                            }
                            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                            _ => {}
                        }
                    }
                    let action = self
                        .logs_browser
                        .as_mut()
                        .map(|b| b.handle_key(key.code, key.modifiers, key.kind))
                        .unwrap_or(crate::logs_browser::LBAction::None);
                    if let crate::logs_browser::LBAction::Edit(path) = action {
                        self.spawn_editor(&path, terminal);
                    }
                }
            }
        }
    }

    fn handle_normal_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        terminal: &mut DefaultTerminal,
    ) -> Result<bool> {
        match (code, mods) {
            (KeyCode::Char('q'), _) => return Ok(true),
            (KeyCode::Esc, _) => {
                if self.crash_view == CrashView::Detail {
                    self.crash_view = CrashView::List;
                    self.status = "? for help".into();
                } else {
                    return Ok(true);
                }
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(true),

            (KeyCode::Char('j') | KeyCode::Down, _) => {
                if self.crash_view == CrashView::Detail {
                    self.preview_scroll = self.preview_scroll.saturating_add(1);
                } else {
                    self.move_selection(1);
                }
            }
            (KeyCode::Char('k') | KeyCode::Up, _) => {
                if self.crash_view == CrashView::Detail {
                    self.preview_scroll = self.preview_scroll.saturating_sub(1);
                } else {
                    self.move_selection(-1);
                }
            }
            (KeyCode::Char('g'), _) => {
                if self.crash_view == CrashView::Detail {
                    self.preview_scroll = 0;
                } else {
                    self.select(0);
                }
            }
            (KeyCode::Char('G'), _) => {
                if self.crash_view == CrashView::Detail {
                    self.preview_scroll = self.preview_line_count().saturating_sub(1);
                } else {
                    self.select(self.visible.len().saturating_sub(1));
                }
            }
            (KeyCode::PageDown, _) => {
                self.preview_scroll = self.preview_scroll.saturating_add(10);
            }
            (KeyCode::PageUp, _) => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
            }

            (KeyCode::Tab, _) => self.switch_tab(1),
            (KeyCode::BackTab, _) => self.switch_tab(-1),

            (KeyCode::Char('r'), _) => self.reload(true),
            (KeyCode::Enter, _) => {
                if self.crash_view == CrashView::List {
                    if self.selected().is_some() {
                        self.crash_view = CrashView::Detail;
                        self.status = "detail \u{2022} tab cycle \u{2022} enter: gdb \u{2022} esc: back".into();
                    }
                } else {
                    self.run_gdb(terminal)?;
                }
            }
            (KeyCode::Char('o'), _) => self.dump_core(),

            (KeyCode::Char('/'), _) => {
                self.mode = Mode::Filter;
                self.status = format!("filter: {}", self.exe_filter);
            }
            (KeyCode::Char('?'), _) => self.mode = Mode::Help,

            (KeyCode::Char('s'), _) => {
                self.sort_mode = self.sort_mode.next();
                self.rebuild_view();
                self.status = format!("sort: {}", self.sort_mode.label());
            }
            (KeyCode::Char('m'), _) => {
                self.grouped = !self.grouped;
                self.rebuild_view();
                self.status = if self.grouped { "grouped by exe".into() } else { "ungrouped".into() };
            }
            (KeyCode::Char('f'), _) => {
                let idx = SIG_FILTER_CYCLE.iter().position(|s| *s == self.sig_filter).unwrap_or(0);
                self.sig_filter = SIG_FILTER_CYCLE[(idx + 1) % SIG_FILTER_CYCLE.len()];
                self.rebuild_view();
                self.status = format!("sig: {}", sig_label(self.sig_filter));
            }
            (KeyCode::Char('t'), _) => {
                self.since_filter = self.since_filter.next();
                self.rebuild_view();
                self.status = format!("since: {}", self.since_filter.label());
            }
            (KeyCode::Char('u'), _) => {
                self.only_failed_unit = !self.only_failed_unit;
                self.rebuild_view();
                self.status = if self.only_failed_unit {
                    "only failed units".into()
                } else {
                    "all units".into()
                };
            }

            (KeyCode::Char('y'), _) => {
                self.mode = Mode::Yank;
                self.status = "yank: p=pid e=exe g=gdb-cmd i=info".into();
            }

            (KeyCode::Char('S'), _) => self.save_report(),
            (KeyCode::Char('x'), _) => self.delete_corefile(),
            (KeyCode::Char('e'), _) => self.open_exe_dir(),

            _ => {}
        }
        Ok(false)
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = "? for help".into();
            }
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.status = format!("filter: '{}'", self.exe_filter);
            }
            KeyCode::Backspace => {
                self.exe_filter.pop();
                self.rebuild_view();
            }
            KeyCode::Char(c) => {
                self.exe_filter.push(c);
                self.rebuild_view();
            }
            _ => {}
        }
    }

    fn handle_yank_key(&mut self, code: KeyCode) {
        let Some(dump) = self.selected().cloned() else {
            self.status = "nothing selected".into();
            return;
        };
        let KeyCode::Char(c) = code else {
            self.status = "yank cancelled".into();
            return;
        };
        let payload = match c {
            'p' => dump.pid.to_string(),
            'e' => dump.exe.clone(),
            'g' => format!("coredumpctl debug {}", dump.pid),
            'i' => self
                .preview_cache
                .get(&(dump.pid, Tab::Info))
                .cloned()
                .unwrap_or_else(|| coredump::info(dump.pid).unwrap_or_default()),
            _ => {
                self.status = "yank cancelled".into();
                return;
            }
        };
        self.status = match coredump::copy_to_clipboard(&payload) {
            Ok(tool) => format!("yanked via {tool}"),
            Err(e) => format!("yank failed: {e}"),
        };
    }

    // ---------- view pipeline ----------

    fn rebuild_view(&mut self) {
        let since_us = match self.since_filter {
            SinceFilter::All => 0,
            SinceFilter::Hour => now_us().saturating_sub(3_600 * 1_000_000),
            SinceFilter::Day => now_us().saturating_sub(86_400 * 1_000_000),
            SinceFilter::Week => now_us().saturating_sub(7 * 86_400 * 1_000_000),
            SinceFilter::Boot => self.boot_time_us,
        };
        let prev_selected_pid = self.selected().map(|d| d.pid);

        let needle = self.exe_filter.to_lowercase();
        let mut dumps: Vec<Dump> = self
            .all
            .iter()
            .filter(|d| d.time >= since_us)
            .filter(|d| match self.sig_filter {
                Some(s) => d.sig == s,
                None => true,
            })
            .filter(|d| needle.is_empty() || d.exe.to_lowercase().contains(&needle))
            .cloned()
            .collect();

        if self.only_failed_unit {
            // Requires a per-dump journal lookup; cheap enough for the visible set.
            dumps.retain(|d| has_failed_unit(d.pid));
        }

        let (visible, counts) = if self.grouped {
            group_by_exe(dumps)
        } else {
            let len = dumps.len();
            (dumps, vec![1; len])
        };

        self.visible = visible;
        self.counts = counts;
        self.apply_sort();

        if let Some(pid) = prev_selected_pid {
            if let Some(i) = self.visible.iter().position(|d| d.pid == pid) {
                self.list_state.select(Some(i));
            } else if !self.visible.is_empty() {
                self.list_state.select(Some(0));
            } else {
                self.list_state.select(None);
            }
        } else if !self.visible.is_empty() {
            self.list_state.select(Some(0));
        } else {
            self.list_state.select(None);
        }
    }

    fn apply_sort(&mut self) {
        let mut paired: Vec<(Dump, usize)> = self
            .visible
            .drain(..)
            .zip(self.counts.drain(..))
            .collect();
        match self.sort_mode {
            SortMode::TimeDesc => paired.sort_by(|a, b| b.0.time.cmp(&a.0.time)),
            SortMode::TimeAsc => paired.sort_by(|a, b| a.0.time.cmp(&b.0.time)),
            SortMode::Exe => paired.sort_by(|a, b| a.0.exe_name().cmp(b.0.exe_name())),
            SortMode::Sig => paired.sort_by(|a, b| a.0.sig.cmp(&b.0.sig)),
            SortMode::SizeDesc => paired.sort_by(|a, b| b.0.size.unwrap_or(0).cmp(&a.0.size.unwrap_or(0))),
        }
        for (d, c) in paired {
            self.visible.push(d);
            self.counts.push(c);
        }
    }

    // ---------- selection / preview ----------

    fn move_selection(&mut self, delta: i32) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let max = self.visible.len() as i32 - 1;
        let next = (cur + delta).clamp(0, max) as usize;
        self.select(next);
    }

    fn select(&mut self, idx: usize) {
        if self.visible.is_empty() {
            return;
        }
        self.list_state.select(Some(idx));
        self.preview_scroll = 0;
        if let Some(d) = self.visible.get(idx) {
            self.new_pids.remove(&d.pid);
        }
    }

    fn selected(&self) -> Option<&Dump> {
        self.list_state.selected().and_then(|i| self.visible.get(i))
    }

    fn switch_tab(&mut self, step: i32) {
        self.tab = self.tab.next(step);
        self.preview_scroll = 0;
    }

    fn ensure_preview(&mut self) -> String {
        let Some(dump) = self.selected().cloned() else {
            return String::new();
        };
        let key = (dump.pid, self.tab);
        if let Some(cached) = self.preview_cache.get(&key) {
            return cached.clone();
        }
        match self.tab {
            Tab::Info => {
                let text = coredump::info(dump.pid).unwrap_or_else(|e| format!("info error: {e}"));
                self.preview_cache.insert(key, text.clone());
                text
            }
            Tab::Env => {
                let map = coredump::journal_fields(dump.pid, &["COREDUMP_ENVIRON"])
                    .unwrap_or_default();
                let text = map
                    .get("COREDUMP_ENVIRON")
                    .cloned()
                    .unwrap_or_else(|| "(no env captured — raise Storage / ProcessSizeMax)".into());
                self.preview_cache.insert(key, text.clone());
                text
            }
            Tab::Cmdline => {
                let map = coredump::journal_fields(dump.pid, &["COREDUMP_CMDLINE"])
                    .unwrap_or_default();
                let text = map
                    .get("COREDUMP_CMDLINE")
                    .cloned()
                    .unwrap_or_else(|| "(no cmdline)".into());
                self.preview_cache.insert(key, text.clone());
                text
            }
            Tab::Logs => {
                let text = coredump::journal_for_dump(&dump)
                    .unwrap_or_else(|e| format!("logs error: {e}"));
                self.preview_cache.insert(key, text.clone());
                text
            }
            Tab::Backtrace => self.ensure_backtrace(&dump),
        }
    }

    fn ensure_backtrace(&mut self, dump: &Dump) -> String {
        let key = (dump.pid, Tab::Backtrace);
        if let Some(cached) = self.preview_cache.get(&key) {
            return cached.clone();
        }
        if self.bt_in_flight == Some(dump.pid) {
            return "(running gdb...)".into();
        }
        // kick off async
        let (tx, rx) = channel();
        let d = dump.clone();
        thread::spawn(move || {
            let res = coredump::backtrace(&d).map_err(|e| e.to_string());
            let _ = tx.send((d.pid, res));
        });
        self.bt_rx = Some(rx);
        self.bt_in_flight = Some(dump.pid);
        self.status = format!("gdb: computing backtrace for pid {}...", dump.pid);
        "(running gdb...)".into()
    }

    fn poll_background(&mut self) {
        let Some(rx) = self.bt_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((pid, Ok(text))) => {
                self.preview_cache.insert((pid, Tab::Backtrace), text);
                self.bt_in_flight = None;
                self.bt_rx = None;
                self.status = format!("gdb done ({pid})");
            }
            Ok((pid, Err(e))) => {
                self.preview_cache.insert((pid, Tab::Backtrace), format!("gdb error: {e}"));
                self.bt_in_flight = None;
                self.bt_rx = None;
                self.status = format!("gdb error ({pid})");
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.bt_rx = None;
                self.bt_in_flight = None;
            }
        }
    }

    // ---------- actions ----------

    fn reload(&mut self, manual: bool) {
        match coredump::list(None) {
            Ok(all) => {
                let old: HashSet<u32> = self.seen_pids.clone();
                for d in &all {
                    if !old.contains(&d.pid) {
                        self.new_pids.insert(d.pid);
                    }
                }
                self.seen_pids = all.iter().map(|d| d.pid).collect();
                self.all = all;
                self.preview_cache.clear();
                self.rebuild_view();
                self.status = if manual {
                    format!("{} crashes", self.all.len())
                } else if self.new_pids.is_empty() {
                    self.status.clone()
                } else {
                    format!("new crash detected ({} pending)", self.new_pids.len())
                };
            }
            Err(e) => self.status = format!("reload error: {e}"),
        }
    }

    fn auto_reload(&mut self) {
        self.last_poll = Instant::now();
        self.reload(false);
    }

    fn preview_line_count(&self) -> u16 {
        let Some(dump) = self.selected() else { return 0 };
        let key = (dump.pid, self.tab);
        self.preview_cache
            .get(&key)
            .map(|s| s.lines().count() as u16)
            .unwrap_or(0)
    }

    fn spawn_editor(&mut self, path: &std::path::Path, terminal: &mut DefaultTerminal) {
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "nano".into());
        let parts: Vec<String> = editor.split_whitespace().map(|s| s.to_owned()).collect();
        if parts.is_empty() {
            return;
        }
        ratatui::restore();
        let status = Command::new(&parts[0])
            .args(&parts[1..])
            .arg(path)
            .status();
        *terminal = ratatui::init();
        let msg = match status {
            Ok(_) => format!("edited {}", path.display()),
            Err(e) => format!("editor failed: {e}"),
        };
        if let Some(b) = self.logs_browser.as_mut() {
            b.set_status(msg);
        }
    }

    fn run_gdb(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let Some(pid) = self.selected().map(|d| d.pid) else {
            return Ok(());
        };
        ratatui::restore();
        let status = Command::new("coredumpctl")
            .arg("debug")
            .arg(pid.to_string())
            .status();
        *terminal = ratatui::init();
        self.status = match status {
            Ok(s) if s.success() => format!("gdb exited cleanly ({pid})"),
            Ok(s) => format!("gdb exited: {s}"),
            Err(e) => format!("gdb failed: {e}"),
        };
        Ok(())
    }

    fn dump_core(&mut self) {
        let Some(dump) = self.selected() else {
            return;
        };
        let path = format!("core.{}", dump.pid);
        let status = Command::new("coredumpctl")
            .args(["dump", &dump.pid.to_string(), "-o", &path])
            .status();
        self.status = match status {
            Ok(s) if s.success() => format!("saved {path}"),
            Ok(s) => format!("dump exited: {s}"),
            Err(e) => format!("dump failed: {e}"),
        };
    }

    fn save_report(&mut self) {
        let Some(dump) = self.selected().cloned() else {
            return;
        };
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = format!("crashout-{}-{}.txt", dump.pid, ts);
        let info = coredump::info(dump.pid).unwrap_or_default();
        let bt = self
            .preview_cache
            .get(&(dump.pid, Tab::Backtrace))
            .cloned()
            .unwrap_or_else(|| String::from("(no backtrace — press Tab to generate)"));
        let body = format!(
            "== crashout report ==\nexe: {}\npid: {}\nsig: {} ({})\ntime: {}\n\n== info ==\n{}\n\n== backtrace ==\n{}\n",
            dump.exe,
            dump.pid,
            dump.sig,
            dump.signal_name(),
            dump.time_string(),
            info,
            bt,
        );
        self.status = match std::fs::write(&path, body) {
            Ok(()) => format!("saved {path}"),
            Err(e) => format!("save failed: {e}"),
        };
    }

    fn delete_corefile(&mut self) {
        let Some(pid) = self.selected().map(|d| d.pid) else {
            return;
        };
        self.status = match coredump::delete_corefile(pid) {
            Ok(path) => {
                self.preview_cache.clear();
                self.reload(true);
                format!("deleted {path}")
            }
            Err(e) => format!("delete failed: {e}"),
        };
    }

    fn open_exe_dir(&mut self) {
        let Some(dump) = self.selected() else {
            return;
        };
        let dir = dump
            .exe
            .rsplit_once('/')
            .map(|(d, _)| d)
            .unwrap_or(&dump.exe);
        let res = Command::new("xdg-open").arg(dir).status();
        self.status = match res {
            Ok(_) => format!("xdg-open {dir}"),
            Err(e) => format!("xdg-open failed: {e}"),
        };
    }

    // ---------- rendering ----------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_top_bar(f, chunks[0]);

        match self.screen {
            Screen::Crashes => {
                self.draw_filter_bar(f, chunks[1]);
                if self.crash_view == CrashView::Detail {
                    self.draw_preview(f, chunks[2]);
                } else {
                    let body = Layout::default()
                        .direction(Direction::Horizontal)
                        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
                        .split(chunks[2]);
                    self.draw_list(f, body[0]);
                    self.draw_preview(f, body[1]);
                }
                self.draw_status(f, chunks[3]);
            }
            Screen::Logs => {
                self.draw_logs_filter_bar(f, chunks[1]);
                if let Some(b) = self.logs_browser.as_mut() {
                    b.draw(f, chunks[2]);
                }
                self.draw_logs_status(f, chunks[3]);
            }
        }

        if self.mode == Mode::Help {
            self.draw_help(f, area);
        }
    }

    fn draw_top_bar(&self, f: &mut Frame, area: Rect) {
        let active = Style::default()
            .bg(Color::Magenta)
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD);
        let inactive = Style::default().fg(Color::Gray);

        let mut spans = vec![
            Span::styled(
                " 1:crashes ",
                if self.screen == Screen::Crashes { active } else { inactive },
            ),
            Span::raw(" "),
            Span::styled(
                " 2:logs ",
                if self.screen == Screen::Logs { active } else { inactive },
            ),
            Span::raw("  "),
        ];

        match self.screen {
            Screen::Crashes => spans.extend(vec![
                Span::styled(format!("{} total", self.all.len()), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(
                    format!("{} shown", self.visible.len()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("sort:{}", self.sort_mode.label()),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("sig:{}", sig_label(self.sig_filter)),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("since:{}", self.since_filter.label()),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  "),
                Span::styled(
                    if self.grouped { "grouped" } else { "flat" },
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  "),
                Span::styled(
                    if self.only_failed_unit { "units:failed" } else { "units:all" },
                    Style::default().fg(Color::Green),
                ),
            ]),
            Screen::Logs => {
                if let Some(b) = &self.logs_browser {
                    spans.push(Span::styled(
                        b.status().to_string(),
                        Style::default().fg(Color::Cyan),
                    ));
                    spans.push(Span::raw("   r:rescan  R:refresh preview"));
                } else {
                    spans.push(Span::raw("loading..."));
                }
            }
        }

        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_logs_filter_bar(&self, f: &mut Frame, area: Rect) {
        let text = match self.logs_browser.as_ref() {
            Some(b) if b.is_filtering() => format!("/{}_", b.filter_text()),
            Some(b) if !b.filter_text().is_empty() => format!("/{}", b.filter_text()),
            _ => String::new(),
        };
        let line = Line::from(vec![Span::styled(text, Style::default().fg(Color::Yellow))]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_logs_status(&self, f: &mut Frame, area: Rect) {
        let msg = self
            .logs_browser
            .as_ref()
            .map(|b| b.status().to_owned())
            .unwrap_or_default();
        let line = Line::from(vec![Span::styled(
            format!(" {} ", msg),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        )]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_filter_bar(&self, f: &mut Frame, area: Rect) {
        let prompt = match self.mode {
            Mode::Filter => format!("/{}_", self.exe_filter),
            _ if !self.exe_filter.is_empty() => format!("/{}", self.exe_filter),
            _ => String::new(),
        };
        let line = Line::from(vec![Span::styled(prompt, Style::default().fg(Color::Yellow))]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .zip(self.counts.iter())
            .map(|(d, count)| {
                let age_style = match d.age() {
                    Age::Hour => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    Age::Day => Style::default().fg(Color::Gray),
                    Age::Week => Style::default().fg(Color::DarkGray),
                    Age::Older => Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
                };
                let sig_color = match d.sig {
                    11 | 7 | 4 => Color::Red,
                    6 => Color::Magenta,
                    _ => Color::Yellow,
                };
                let corefile = if d.corefile_present() {
                    Span::styled("\u{25CF}", Style::default().fg(Color::Green))
                } else {
                    Span::styled("\u{25CB}", Style::default().fg(Color::DarkGray))
                };
                let new_flag = if self.new_pids.contains(&d.pid) {
                    Span::styled(
                        " NEW ",
                        Style::default().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw("")
                };
                let count_span = if *count > 1 {
                    Span::styled(format!(" x{count}"), Style::default().fg(Color::Yellow))
                } else {
                    Span::raw("")
                };
                ListItem::new(Line::from(vec![
                    corefile,
                    Span::raw(" "),
                    Span::styled(d.time_string(), age_style),
                    Span::raw("  "),
                    Span::styled(
                        format!("{:<8}", d.signal_name()),
                        Style::default().fg(sig_color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(d.exe_name().to_string(), Style::default().fg(Color::Cyan)),
                    count_span,
                    Span::raw("  "),
                    Span::styled(
                        format!("{:>7}", d.size_human()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("pid {}", d.pid),
                        Style::default().fg(Color::DarkGray),
                    ),
                    new_flag,
                ]))
            })
            .collect();

        let title = format!(" crashes ({}) ", self.visible.len());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        let titles: Vec<Line> = Tab::ORDER
            .iter()
            .map(|t| Line::from(t.label()))
            .collect();
        let active = Tab::ORDER.iter().position(|t| *t == self.tab).unwrap_or(0);
        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::ALL).title(" preview (tab) "))
            .select(active)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_widget(tabs, chunks[0]);

        let para = if self.visible.is_empty() {
            Paragraph::new("no coredumps match current filters")
                .block(Block::default().borders(Borders::ALL))
                .wrap(Wrap { trim: false })
                .scroll((self.preview_scroll, 0))
        } else {
            let text = self.ensure_preview();
            if self.tab == Tab::Logs {
                Paragraph::new(logview::colorize(&text))
                    .block(Block::default().borders(Borders::ALL))
                    .wrap(Wrap { trim: false })
                    .scroll((self.preview_scroll, 0))
            } else {
                Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL))
                    .wrap(Wrap { trim: false })
                    .scroll((self.preview_scroll, 0))
            }
        };
        f.render_widget(para, chunks[1]);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let new_hint = if !self.new_pids.is_empty() {
            format!("  \u{2728} {} new", self.new_pids.len())
        } else {
            String::new()
        };
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::styled(new_hint, Style::default().fg(Color::Yellow)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) {
        let width = 70.min(area.width.saturating_sub(4));
        let height = 32.min(area.height.saturating_sub(4));
        let x = area.x + (area.width - width) / 2;
        let y = area.y + (area.height - height) / 2;
        let rect = Rect { x, y, width, height };

        f.render_widget(Clear, rect);
        let help_lines = vec![
            "-- global --",
            "1 / 2        switch screen: crashes / logs",
            "? / Esc      this help / close help (any key)",
            "q            quit",
            "",
            "-- crashes screen --",
            "j/k  \u{2193}/\u{2191}   navigate",
            "g / G        top / bottom",
            "PgUp/PgDn    scroll preview",
            "tab / S-tab  cycle preview (info/bt/logs/env/cmdline)",
            "enter        list \u{2192} detail fullscreen \u{2192} gdb",
            "esc          detail \u{2192} list (top level = quit)",
            "o            dump core to ./core.<pid>",
            "S            save report",
            "x            delete corefile from disk",
            "e            xdg-open exe directory",
            "/            filter by exe name (Esc clears)",
            "s m f t u    sort/group/sig/since/units toggles",
            "y then p/e/g/i   yank: pid/exe/gdb-cmd/info",
            "r            manual reload (auto every 2s)",
            "",
            "-- logs screen --",
            "j/k g/G      navigate sources (browser) or lines (fullscreen)",
            "PgUp/PgDn    scroll",
            "enter        browser \u{2192} fullscreen \u{2192} open in $EDITOR",
            "esc          fullscreen \u{2192} browser (top level = quit)",
            "/            filter sources",
            "r / R        rescan all / refresh preview",
        ];
        let text: Vec<Line> = help_lines.into_iter().map(Line::from).collect();
        let para = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" keybinds (any key to close) "),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(para, rect);
    }
}

// ---------- helpers ----------

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn boot_time_us() -> u64 {
    let uptime = std::fs::read_to_string("/proc/uptime").unwrap_or_default();
    let up: f64 = uptime
        .split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    (((now - up).max(0.0)) * 1_000_000.0) as u64
}

fn has_failed_unit(pid: u32) -> bool {
    coredump::journal_fields(pid, &["COREDUMP_UNIT"])
        .map(|m| m.get("COREDUMP_UNIT").map(|s| !s.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

fn group_by_exe(dumps: Vec<Dump>) -> (Vec<Dump>, Vec<usize>) {
    let mut by: HashMap<String, (Dump, usize)> = HashMap::new();
    for d in dumps {
        by.entry(d.exe.clone())
            .and_modify(|(rep, count)| {
                if d.time > rep.time {
                    *rep = d.clone();
                }
                *count += 1;
            })
            .or_insert((d, 1));
    }
    let mut out_dumps = Vec::with_capacity(by.len());
    let mut out_counts = Vec::with_capacity(by.len());
    for (_, (d, c)) in by {
        out_dumps.push(d);
        out_counts.push(c);
    }
    (out_dumps, out_counts)
}
