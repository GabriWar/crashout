use crate::logview;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum LBMode {
    Browser,
    Fullscreen,
}

/// Action requested by the logs browser that the main app must perform
/// (because it needs terminal-level control).
pub enum LBAction {
    None,
    Edit(PathBuf),
}

const PREVIEW_LINES: usize = 500;
const MAX_FILES: usize = 2000;
const MAX_DEPTH: u32 = 6;

#[derive(Clone)]
pub enum Source {
    File { path: PathBuf, size: u64, mtime: u64 },
    Unit(String),
    UserUnit(String),
    Kernel,
    All,
}

impl Source {
    fn kind_tag(&self) -> &'static str {
        match self {
            Source::File { .. } => "[FILE]",
            Source::Unit(_) => "[UNIT]",
            Source::UserUnit(_) => "[USER]",
            Source::Kernel => "[KERN]",
            Source::All => "[ALL] ",
        }
    }

    fn tag_style(&self) -> Style {
        match self {
            Source::File { .. } => Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Source::Unit(_) => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            Source::UserUnit(_) => Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
            Source::Kernel => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Source::All => Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        }
    }

    fn match_text(&self) -> String {
        match self {
            Source::File { path, .. } => path.display().to_string(),
            Source::Unit(u) | Source::UserUnit(u) => u.clone(),
            Source::Kernel => "kernel dmesg".into(),
            Source::All => "full journal".into(),
        }
    }

    fn sort_key(&self) -> (u8, i64, String) {
        match self {
            Source::File { mtime, .. } => (0, -(*mtime as i64), self.match_text()),
            Source::All => (1, 0, String::new()),
            Source::Kernel => (2, 0, String::new()),
            Source::Unit(_) => (3, 0, self.match_text()),
            Source::UserUnit(_) => (4, 0, self.match_text()),
        }
    }

    fn fetch_preview(&self) -> String {
        match self {
            Source::File { path, .. } => tail_file(path),
            Source::Unit(u) => journal_unit(u, false),
            Source::UserUnit(u) => journal_unit(u, true),
            Source::Kernel => journal_args(&["-k", "-n", &PREVIEW_LINES.to_string(), "-o", "short-iso", "--no-pager"]),
            Source::All => journal_args(&["-n", &PREVIEW_LINES.to_string(), "-o", "short-iso", "--no-pager"]),
        }
    }
}

pub struct LogsBrowser {
    sources: Vec<Source>,
    visible: Vec<usize>,
    list_state: ListState,
    preview: Vec<Line<'static>>,
    preview_of: Option<usize>,
    preview_scroll: u16,
    filter: String,
    filtering: bool,
    status: String,

    mode: LBMode,
    fs_list_state: ListState,
}

impl LogsBrowser {
    pub fn new() -> Self {
        let mut sources = discover();
        sources.sort_by_key(|s| s.sort_key());
        let mut b = Self {
            sources,
            visible: Vec::new(),
            list_state: ListState::default(),
            preview: Vec::new(),
            preview_of: None,
            preview_scroll: 0,
            filter: String::new(),
            filtering: false,
            status: String::new(),
            mode: LBMode::Browser,
            fs_list_state: ListState::default(),
        };
        b.apply_filter();
        b.status = format!("{} log sources", b.sources.len());
        b.ensure_preview();
        b
    }

    pub fn is_filtering(&self) -> bool {
        self.filtering
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn set_status(&mut self, s: String) {
        self.status = s;
    }

    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    pub fn mode(&self) -> LBMode {
        self.mode
    }

    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers, kind: KeyEventKind) -> LBAction {
        if kind != KeyEventKind::Press {
            return LBAction::None;
        }
        if self.filtering {
            match code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.filtering = false;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.apply_filter();
                    self.ensure_preview();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.apply_filter();
                    self.ensure_preview();
                }
                _ => {}
            }
            return LBAction::None;
        }
        match self.mode {
            LBMode::Browser => self.handle_browser_key(code, mods),
            LBMode::Fullscreen => self.handle_fullscreen_key(code, mods),
        }
    }

    fn handle_browser_key(&mut self, code: KeyCode, _mods: KeyModifiers) -> LBAction {
        match code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') => self.select(0),
            KeyCode::Char('G') => self.select(self.visible.len().saturating_sub(1)),
            KeyCode::PageDown => {
                self.preview_scroll = self.preview_scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_sub(10);
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('r') => {
                self.status = "rescanning...".into();
                let mut sources = discover();
                sources.sort_by_key(|s| s.sort_key());
                self.sources = sources;
                self.apply_filter();
                self.preview_of = None;
                self.preview.clear();
                self.ensure_preview();
                self.status = format!("{} log sources", self.sources.len());
            }
            KeyCode::Char('R') => {
                self.preview_of = None;
                self.ensure_preview();
                self.status = "preview refreshed".into();
            }
            KeyCode::Enter => {
                self.ensure_preview();
                if self.preview_of.is_some() && !self.preview.is_empty() {
                    self.mode = LBMode::Fullscreen;
                    self.fs_list_state.select(Some(0));
                    self.status = "fullscreen \u{2022} j/k scroll \u{2022} enter edit \u{2022} esc back".into();
                }
            }
            _ => {}
        }
        LBAction::None
    }

    fn handle_fullscreen_key(&mut self, code: KeyCode, _mods: KeyModifiers) -> LBAction {
        match code {
            KeyCode::Esc => {
                self.mode = LBMode::Browser;
                self.status = format!("{} log sources", self.sources.len());
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_fs(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_fs(-1),
            KeyCode::PageDown => self.move_fs(10),
            KeyCode::PageUp => self.move_fs(-10),
            KeyCode::Char('g') => self.fs_list_state.select(Some(0)),
            KeyCode::Char('G') => {
                if !self.preview.is_empty() {
                    self.fs_list_state.select(Some(self.preview.len() - 1));
                }
            }
            KeyCode::Char('R') => {
                self.preview_of = None;
                self.ensure_preview();
                self.status = "preview refreshed".into();
            }
            KeyCode::Enter => {
                if let Some(src_idx) = self.preview_of {
                    if let Some(src) = self.sources.get(src_idx).cloned() {
                        match edit_target(&src) {
                            Ok(path) => return LBAction::Edit(path),
                            Err(e) => self.status = format!("edit prep failed: {e}"),
                        }
                    }
                }
            }
            _ => {}
        }
        LBAction::None
    }

    fn move_fs(&mut self, delta: i32) {
        if self.preview.is_empty() {
            return;
        }
        let max = self.preview.len() as i32 - 1;
        let cur = self.fs_list_state.selected().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, max) as usize;
        self.fs_list_state.select(Some(next));
    }

    fn move_selection(&mut self, delta: i32) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let max = self.visible.len() as i32 - 1;
        let next = (cur + delta).clamp(0, max) as usize;
        self.select(next);
    }

    fn select(&mut self, i: usize) {
        if self.visible.is_empty() {
            return;
        }
        self.list_state.select(Some(i));
        self.preview_scroll = 0;
        self.ensure_preview();
    }

    fn apply_filter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.visible = self
            .sources
            .iter()
            .enumerate()
            .filter(|(_, s)| needle.is_empty() || s.match_text().to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if self.visible.is_empty() {
            self.list_state.select(None);
        } else {
            let cur = self.list_state.selected().unwrap_or(0).min(self.visible.len() - 1);
            self.list_state.select(Some(cur));
        }
    }

    fn ensure_preview(&mut self) {
        let Some(vis_idx) = self.list_state.selected() else {
            self.preview.clear();
            self.preview_of = None;
            return;
        };
        let Some(&src_idx) = self.visible.get(vis_idx) else {
            return;
        };
        if self.preview_of == Some(src_idx) {
            return;
        }
        let Some(src) = self.sources.get(src_idx) else {
            return;
        };
        let text = src.fetch_preview();
        self.preview = logview::colorize(&text);
        self.preview_of = Some(src_idx);
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        match self.mode {
            LBMode::Browser => {
                let body = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
                    .split(area);
                self.draw_list(f, body[0]);
                self.draw_preview(f, body[1]);
            }
            LBMode::Fullscreen => {
                self.draw_fullscreen(f, area);
            }
        }
    }

    fn draw_fullscreen(&mut self, f: &mut Frame, area: Rect) {
        let title = match self.preview_of.and_then(|i| self.sources.get(i)) {
            Some(s) => format!(" {}  (enter: edit \u{2022} esc: back) ", s.match_text()),
            None => String::from(" log "),
        };
        let items: Vec<ListItem> = self
            .preview
            .iter()
            .map(|l| ListItem::new(l.clone()))
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("\u{25B6} ");
        f.render_stateful_widget(list, area, &mut self.fs_list_state);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .filter_map(|i| self.sources.get(*i))
            .map(|s| {
                let (size_str, meta_str) = match s {
                    Source::File { size, mtime, .. } => (human_bytes(*size), time_rel(*mtime)),
                    _ => (String::new(), String::new()),
                };
                ListItem::new(Line::from(vec![
                    Span::styled(s.kind_tag(), s.tag_style()),
                    Span::raw(" "),
                    Span::styled(format!("{:>8}", size_str), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(format!("{:<7}", meta_str), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::raw(s.match_text()),
                ]))
            })
            .collect();

        let title = format!(" sources ({}/{}) ", self.visible.len(), self.sources.len());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");
        f.render_stateful_widget(list, area, &mut self.list_state);
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let title = match self.preview_of.and_then(|i| self.sources.get(i)) {
            Some(s) => format!(" preview: {} ", s.match_text()),
            None => String::from(" preview "),
        };
        let para = Paragraph::new(self.preview.clone())
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll, 0));
        f.render_widget(para, area);
    }
}

// ---------- discovery ----------

fn discover() -> Vec<Source> {
    let mut out = Vec::new();
    out.push(Source::All);
    out.push(Source::Kernel);
    collect_units(&mut out, false);
    collect_units(&mut out, true);
    collect_files(&mut out);
    out
}

fn collect_units(out: &mut Vec<Source>, user: bool) {
    let mut cmd = Command::new("journalctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args(["-F", "_SYSTEMD_UNIT", "--no-pager"]);
    let Ok(output) = cmd.output() else { return };
    if !output.status.success() {
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        if user {
            out.push(Source::UserUnit(name.to_owned()));
        } else {
            out.push(Source::Unit(name.to_owned()));
        }
    }
}

fn collect_files(out: &mut Vec<Source>) {
    let mut roots: Vec<PathBuf> = vec![PathBuf::from("/var/log"), PathBuf::from("/run/log")];
    if let Ok(home) = std::env::var("HOME") {
        let h = PathBuf::from(home);
        roots.push(h.join(".local/share"));
        roots.push(h.join(".local/state"));
        roots.push(h.join(".cache"));
        roots.push(h.join(".config"));
    }
    let mut count = 0;
    for root in &roots {
        if count >= MAX_FILES {
            break;
        }
        walk(root, MAX_DEPTH, &mut count, out, is_under_varlog(root));
    }
}

fn is_under_varlog(p: &Path) -> bool {
    p.starts_with("/var/log") || p.starts_with("/run/log")
}

fn walk(dir: &Path, depth: u32, count: &mut usize, out: &mut Vec<Source>, liberal: bool) {
    if depth == 0 || *count >= MAX_FILES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *count >= MAX_FILES {
            break;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if ft.is_dir() {
            if name.starts_with('.') && !liberal {
                continue;
            }
            if is_useless_dir(name) {
                continue;
            }
            walk(&path, depth - 1, count, out, liberal || dir_is_logdir(name));
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        if !looks_like_log(name, liberal) {
            continue;
        }
        if is_binary_log_name(name) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let size = meta.len();
        if size == 0 {
            continue;
        }
        if looks_binary(&path) {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(Source::File {
            path: path.clone(),
            size,
            mtime,
        });
        *count += 1;
    }
}

fn is_binary_log_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // Known binary log formats.
    if lower.ends_with(".journal")
        || lower.ends_with(".journal~")
        || lower.ends_with(".db")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite3")
        || lower.ends_with(".idx")
        || lower.ends_with(".lmdb")
        || lower.ends_with(".bin")
        || lower.ends_with(".pcap")
        || lower.ends_with(".pcapng")
    {
        return true;
    }
    matches!(
        name,
        "wtmp" | "btmp" | "utmp" | "lastlog" | "tallylog" | "faillog"
    )
}

/// Peek the first 512 bytes for null bytes (crude binary sniff).
fn looks_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return true;
    };
    let mut buf = [0u8; 512];
    match f.read(&mut buf) {
        Ok(0) => false,
        Ok(n) => buf[..n].contains(&0),
        Err(_) => true,
    }
}

fn looks_like_log(name: &str, liberal: bool) -> bool {
    let lower = name.to_ascii_lowercase();
    if is_compressed(&lower) {
        return false;
    }
    if liberal {
        return true;
    }
    if lower.contains(".log") || lower.ends_with(".out") || lower.ends_with(".err") {
        return true;
    }
    false
}

fn is_compressed(name: &str) -> bool {
    matches!(
        name.rsplit('.').next(),
        Some("gz") | Some("xz") | Some("zst") | Some("bz2") | Some("lz4") | Some("zip")
    )
}

fn dir_is_logdir(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l == "log" || l == "logs"
}

fn is_useless_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | "target"
            | ".git"
            | ".svn"
            | "cargo-target"
            | "build"
            | "dist"
            | ".venv"
            | "venv"
            | "__pycache__"
            | "journal"
    )
}

// ---------- fetchers ----------

fn tail_file(path: &Path) -> String {
    let out = Command::new("tail")
        .arg("-n")
        .arg(PREVIEW_LINES.to_string())
        .arg(path)
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => format!(
            "tail failed: {}\nstderr: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => format!("tail spawn failed: {e}"),
    }
}

fn journal_unit(unit: &str, user: bool) -> String {
    let mut cmd = Command::new("journalctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args([
        "-u",
        unit,
        "-n",
        &PREVIEW_LINES.to_string(),
        "-o",
        "short-iso",
        "--no-pager",
    ]);
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => format!("journalctl spawn failed: {e}"),
    }
}

fn journal_args(args: &[&str]) -> String {
    match Command::new("journalctl").args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => format!("journalctl spawn failed: {e}"),
    }
}

// ---------- formatting ----------

fn human_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let v = b as f64;
    if v < K {
        format!("{b}B")
    } else if v < K * K {
        format!("{:.1}K", v / K)
    } else if v < K * K * K {
        format!("{:.1}M", v / (K * K))
    } else {
        format!("{:.1}G", v / (K * K * K))
    }
}

/// Produce a path that an editor can open for the given source.
/// For File sources, that's the path itself. For journal-backed sources,
/// dump the relevant `journalctl` output to a temp file first.
fn edit_target(src: &Source) -> anyhow::Result<PathBuf> {
    match src {
        Source::File { path, .. } => Ok(path.clone()),
        other => {
            let (name, text) = match other {
                Source::Unit(u) => (sanitize(u), journal_full_unit(u, false)),
                Source::UserUnit(u) => (format!("user-{}", sanitize(u)), journal_full_unit(u, true)),
                Source::Kernel => (
                    "kernel".into(),
                    journal_args(&["-k", "--no-pager", "-o", "short-iso"]),
                ),
                Source::All => (
                    "all".into(),
                    journal_args(&["-n", "5000", "--no-pager", "-o", "short-iso"]),
                ),
                Source::File { .. } => unreachable!(),
            };
            let path = std::env::temp_dir().join(format!("crashout-{name}.log"));
            let mut f = std::fs::File::create(&path)?;
            f.write_all(text.as_bytes())?;
            Ok(path)
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn journal_full_unit(unit: &str, user: bool) -> String {
    let mut cmd = Command::new("journalctl");
    if user {
        cmd.arg("--user");
    }
    cmd.args(["-u", unit, "--no-pager", "-o", "short-iso"]);
    match cmd.output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(e) => format!("journalctl spawn failed: {e}"),
    }
}

fn time_rel(mtime: u64) -> String {
    if mtime == 0 {
        return String::new();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let diff = now.saturating_sub(mtime);
    if diff < 60 {
        format!("{diff}s")
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else if diff < 86_400 * 30 {
        format!("{}d", diff / 86_400)
    } else {
        format!("{}mo", diff / (86_400 * 30))
    }
}
