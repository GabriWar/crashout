use crate::coredump;
use crate::procs::{self, History, ProcInfo, Stream};
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Sparkline, Tabs, Wrap},
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DTab {
    Status,
    Maps,
    Fds,
    Limits,
    Environ,
    Stream,
}

impl DTab {
    pub const ORDER: [DTab; 6] = [
        DTab::Status,
        DTab::Maps,
        DTab::Fds,
        DTab::Limits,
        DTab::Environ,
        DTab::Stream,
    ];
    fn label(self) -> &'static str {
        match self {
            DTab::Status => "status",
            DTab::Maps => "maps",
            DTab::Fds => "fds",
            DTab::Limits => "limits",
            DTab::Environ => "environ",
            DTab::Stream => "stream",
        }
    }
    fn step(self, d: i32) -> DTab {
        let i = Self::ORDER.iter().position(|t| *t == self).unwrap_or(0) as i32;
        let n = Self::ORDER.len() as i32;
        Self::ORDER[((i + d).rem_euclid(n)) as usize]
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    CpuDesc,
    MemDesc,
    PidAsc,
    NameAsc,
}
impl SortMode {
    fn next(self) -> Self {
        match self {
            Self::CpuDesc => Self::MemDesc,
            Self::MemDesc => Self::PidAsc,
            Self::PidAsc => Self::NameAsc,
            Self::NameAsc => Self::CpuDesc,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::CpuDesc => "cpu\u{2193}",
            Self::MemDesc => "mem\u{2193}",
            Self::PidAsc => "pid\u{2191}",
            Self::NameAsc => "name",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    List,
    Detail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Filter,
}

pub struct ProcsScreen {
    procs: Vec<ProcInfo>,
    visible: Vec<usize>,
    list_state: ListState,
    history: History,
    last_poll: Instant,

    view: View,
    tab: DTab,
    detail_scroll: u16,
    cached_pid: Option<u32>,
    cached_tab: Option<DTab>,
    cached_text: String,
    stream: Option<Stream>,

    sort: SortMode,
    filter: String,
    mode: Mode,
    pub status: String,
}

impl ProcsScreen {
    pub fn new() -> Self {
        let mut s = Self {
            procs: Vec::new(),
            visible: Vec::new(),
            list_state: ListState::default(),
            history: History::new(),
            last_poll: Instant::now() - Duration::from_secs(60),
            view: View::List,
            tab: DTab::Status,
            detail_scroll: 0,
            cached_pid: None,
            cached_tab: None,
            cached_text: String::new(),
            stream: None,
            sort: SortMode::CpuDesc,
            filter: String::new(),
            mode: Mode::Normal,
            status: "? for help".into(),
        };
        s.refresh();
        s
    }

    pub fn tick(&mut self) {
        if self.last_poll.elapsed() >= Duration::from_secs(2) {
            self.refresh();
        }
        if let Some(stream) = self.stream.as_mut() {
            stream.drain();
        }
    }

    pub fn is_filtering(&self) -> bool {
        self.mode == Mode::Filter
    }

    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    fn refresh(&mut self) {
        self.last_poll = Instant::now();
        match procs::list() {
            Ok(mut p) => {
                p.sort_by_key(|p| p.pid);
                self.history.record(&p);
                let prev_pid = self.selected_pid();
                self.procs = p;
                self.rebuild_view();
                if let Some(pid) = prev_pid {
                    if let Some(i) = self.visible.iter().position(|&i| self.procs[i].pid == pid) {
                        self.list_state.select(Some(i));
                    }
                }
            }
            Err(e) => self.status = format!("proc scan failed: {e}"),
        }
    }

    fn rebuild_view(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut idx: Vec<usize> = self
            .procs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                if needle.is_empty() {
                    true
                } else {
                    p.comm.to_lowercase().contains(&needle)
                        || p.cmdline.to_lowercase().contains(&needle)
                        || p.pid.to_string().contains(&needle)
                }
            })
            .map(|(i, _)| i)
            .collect();

        match self.sort {
            SortMode::CpuDesc => {
                idx.sort_by(|&a, &b| {
                    self.history
                        .last_cpu(self.procs[b].pid)
                        .cmp(&self.history.last_cpu(self.procs[a].pid))
                        .then(self.procs[b].cpu_ticks.cmp(&self.procs[a].cpu_ticks))
                });
            }
            SortMode::MemDesc => idx.sort_by(|&a, &b| self.procs[b].rss_kb.cmp(&self.procs[a].rss_kb)),
            SortMode::PidAsc => idx.sort_by_key(|&i| self.procs[i].pid),
            SortMode::NameAsc => idx.sort_by(|&a, &b| self.procs[a].comm.cmp(&self.procs[b].comm)),
        }
        self.visible = idx;
        if self.list_state.selected().is_none() && !self.visible.is_empty() {
            self.list_state.select(Some(0));
        }
    }

    fn selected_pid(&self) -> Option<u32> {
        let i = self.list_state.selected()?;
        let &p_idx = self.visible.get(i)?;
        Some(self.procs.get(p_idx)?.pid)
    }

    fn selected(&self) -> Option<&ProcInfo> {
        let i = self.list_state.selected()?;
        let &p_idx = self.visible.get(i)?;
        self.procs.get(p_idx)
    }

    fn move_sel(&mut self, d: i32) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let max = self.visible.len() as i32 - 1;
        let next = (cur + d).clamp(0, max) as usize;
        self.list_state.select(Some(next));
        self.detail_scroll = 0;
        // If a stream was running, kill it — selection changed.
        if self.stream.is_some() {
            self.stream = None;
            self.status = "stream stopped (selection changed)".into();
        }
        self.cached_pid = None;
    }

    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // Returns true to request quit.
        if self.mode == Mode::Filter {
            match code {
                KeyCode::Esc | KeyCode::Enter => self.mode = Mode::Normal,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.rebuild_view();
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.rebuild_view();
                }
                _ => {}
            }
            return false;
        }
        match (code, mods) {
            (KeyCode::Char('q'), _) => return true,
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return true,
            (KeyCode::Esc, _) => {
                if self.view == View::Detail {
                    self.view = View::List;
                    self.stream = None;
                    self.status = "? for help".into();
                } else {
                    return true;
                }
            }
            (KeyCode::Char('j') | KeyCode::Down, _) => {
                if self.view == View::Detail {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                } else {
                    self.move_sel(1);
                }
            }
            (KeyCode::Char('k') | KeyCode::Up, _) => {
                if self.view == View::Detail {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                } else {
                    self.move_sel(-1);
                }
            }
            (KeyCode::PageDown, _) => self.detail_scroll = self.detail_scroll.saturating_add(10),
            (KeyCode::PageUp, _) => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            (KeyCode::Char('g'), _) => {
                if self.view == View::Detail {
                    self.detail_scroll = 0;
                } else if !self.visible.is_empty() {
                    self.list_state.select(Some(0));
                }
            }
            (KeyCode::Char('G'), _) => {
                if self.view == View::Detail {
                    self.detail_scroll = u16::MAX / 2;
                } else if !self.visible.is_empty() {
                    self.list_state.select(Some(self.visible.len() - 1));
                }
            }
            (KeyCode::Tab, _) => {
                self.tab = self.tab.step(1);
                self.detail_scroll = 0;
                self.cached_pid = None;
                if self.tab != DTab::Stream {
                    self.stream = None;
                }
            }
            (KeyCode::BackTab, _) => {
                self.tab = self.tab.step(-1);
                self.detail_scroll = 0;
                self.cached_pid = None;
                if self.tab != DTab::Stream {
                    self.stream = None;
                }
            }
            (KeyCode::Enter, _) => {
                if self.view == View::List {
                    if self.selected().is_some() {
                        self.view = View::Detail;
                        self.status = "detail \u{2022} tab cycle \u{2022} esc back \u{2022} y yank".into();
                    }
                } else if self.tab == DTab::Stream {
                    self.toggle_stream();
                }
            }
            (KeyCode::Char('/'), _) => {
                self.mode = Mode::Filter;
                self.status = format!("filter: {}", self.filter);
            }
            (KeyCode::Char('s'), _) => {
                self.sort = self.sort.next();
                self.rebuild_view();
                self.status = format!("sort: {}", self.sort.label());
            }
            (KeyCode::Char('r'), _) => self.refresh(),
            (KeyCode::Char('K'), _) => self.kill_selected(15),
            (KeyCode::Char('9'), _) => self.kill_selected(9),
            (KeyCode::Char('y'), _) => self.yank_pid(),
            _ => {}
        }
        false
    }

    fn toggle_stream(&mut self) {
        let Some(pid) = self.selected_pid() else { return };
        if self.stream.as_ref().map(|s| s.pid) == Some(pid) {
            self.stream = None;
            self.status = format!("stream stopped (pid {pid})");
            return;
        }
        match Stream::start(pid) {
            Ok(s) => {
                self.stream = Some(s);
                self.status = format!("strace pid {pid} (Enter again to stop)");
            }
            Err(e) => self.status = format!("strace failed: {e}"),
        }
    }

    fn kill_selected(&mut self, sig: i32) {
        let Some(pid) = self.selected_pid() else { return };
        // SAFETY: kill is just a syscall wrapper; pid validated >0 by /proc enum.
        let rc = unsafe { libc_kill(pid as i32, sig) };
        self.status = if rc == 0 {
            format!("sent sig {sig} \u{2192} pid {pid}")
        } else {
            format!("kill failed (perm? signal {sig} pid {pid})")
        };
        self.refresh();
    }

    fn yank_pid(&mut self) {
        let Some(pid) = self.selected_pid() else { return };
        self.status = match coredump::copy_to_clipboard(&pid.to_string()) {
            Ok(t) => format!("yanked pid {pid} via {t}"),
            Err(e) => format!("yank failed: {e}"),
        };
    }

    // ---------- rendering ----------

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        match self.view {
            View::List => self.draw_list_view(f, area),
            View::Detail => self.draw_detail_view(f, area),
        }
    }

    fn draw_list_view(&mut self, f: &mut Frame, area: Rect) {
        // top: list. bottom: sparkline of selected.
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(7)])
            .split(area);

        let items: Vec<ListItem> = self
            .visible
            .iter()
            .filter_map(|&i| self.procs.get(i))
            .map(|p| {
                let cpu = self.history.last_cpu(p.pid);
                let cpu_color = if cpu >= 50 {
                    Color::Red
                } else if cpu >= 20 {
                    Color::Yellow
                } else {
                    Color::Green
                };
                let state_color = match p.state {
                    'R' => Color::Green,
                    'D' => Color::Red,
                    'Z' => Color::Magenta,
                    'T' | 't' => Color::Yellow,
                    _ => Color::Gray,
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>6}", p.pid), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(format!("{:>5}", p.ppid), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(format!("{}", p.state), Style::default().fg(state_color).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(format!("{cpu:>3}%"), Style::default().fg(cpu_color).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled(format!("{:>8}", p.rss_human()), Style::default().fg(Color::Cyan)),
                    Span::raw(" "),
                    Span::styled(format!("{:>3}t", p.threads), Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::styled(format!("{:>7}", p.cpu_time_str()), Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::raw(truncate(p.display_name(), 80)),
                ]))
            })
            .collect();
        let title = format!(" procs ({}/{}) ", self.visible.len(), self.procs.len());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("> ");
        f.render_stateful_widget(list, chunks[0], &mut self.list_state);

        self.draw_sparklines(f, chunks[1]);
    }

    fn draw_sparklines(&self, f: &mut Frame, area: Rect) {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        let (cpu, rss, label) = match self.selected_pid() {
            Some(pid) => (
                self.history.cpu_series(pid),
                self.history.rss_series(pid),
                format!("pid {pid}"),
            ),
            None => (Vec::new(), Vec::new(), "(no selection)".into()),
        };
        let cpu_max = cpu.iter().copied().max().unwrap_or(100).max(10);
        let rss_max = rss.iter().copied().max().unwrap_or(0).max(1);
        let cpu_spark = Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" cpu% \u{2022} {label} ")),
            )
            .data(&cpu)
            .max(cpu_max)
            .style(Style::default().fg(Color::Green))
            .bar_set(symbols::bar::NINE_LEVELS);
        let rss_spark = Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" rss \u{2022} max {} ", human_kb(rss_max))),
            )
            .data(&rss)
            .max(rss_max)
            .style(Style::default().fg(Color::Cyan))
            .bar_set(symbols::bar::NINE_LEVELS);
        f.render_widget(cpu_spark, split[0]);
        f.render_widget(rss_spark, split[1]);
    }

    fn draw_detail_view(&mut self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        let titles: Vec<Line> = DTab::ORDER.iter().map(|t| Line::from(t.label())).collect();
        let active = DTab::ORDER.iter().position(|t| *t == self.tab).unwrap_or(0);
        let header = match self.selected() {
            Some(p) => format!(" pid {} \u{2022} {} ", p.pid, p.comm),
            None => " (no selection) ".into(),
        };
        let tabs = Tabs::new(titles)
            .block(Block::default().borders(Borders::ALL).title(header))
            .select(active)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        f.render_widget(tabs, chunks[0]);

        let pid = match self.selected_pid() {
            Some(p) => p,
            None => {
                let p = Paragraph::new("no process selected").block(Block::default().borders(Borders::ALL));
                f.render_widget(p, chunks[1]);
                return;
            }
        };

        let text = self.detail_text(pid);
        let title = format!(" {} ", self.tab.label());
        let para = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.detail_scroll, 0));
        f.render_widget(para, chunks[1]);
    }

    fn detail_text(&mut self, pid: u32) -> String {
        if self.tab == DTab::Stream {
            return match self.stream.as_ref() {
                Some(s) if s.pid == pid => {
                    let body = s.text();
                    if body.is_empty() {
                        "(strace running... waiting for syscalls)".into()
                    } else {
                        body
                    }
                }
                _ => "(press Enter to start strace on this pid)\n\nrequires `strace` in PATH and ptrace permission (run crashout under `sudo` or set kernel.yama.ptrace_scope=0).".into(),
            };
        }
        if self.cached_pid == Some(pid) && self.cached_tab == Some(self.tab) {
            return self.cached_text.clone();
        }
        let text = match self.tab {
            DTab::Status => procs::status(pid),
            DTab::Maps => procs::maps(pid),
            DTab::Fds => procs::fds(pid),
            DTab::Limits => procs::limits(pid),
            DTab::Environ => procs::environ(pid),
            DTab::Stream => unreachable!(),
        };
        self.cached_pid = Some(pid);
        self.cached_tab = Some(self.tab);
        self.cached_text = text.clone();
        text
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('\u{2026}');
        t
    }
}

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

// libc::kill without pulling the libc crate.
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe { kill(pid, sig) }
}
