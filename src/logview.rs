use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Level {
    None,
    Trace,
    Debug,
    Info,
    Notice,
    Warn,
    Error,
    Critical,
    Fatal,
}

/// Check keywords in priority order (severe first). First hit wins.
/// Keywords are matched with ASCII word boundaries so "err" doesn't hit "error".
const KEYWORDS: &[(&str, Level)] = &[
    ("panic", Level::Fatal),
    ("fatal", Level::Fatal),
    ("emerg", Level::Fatal),
    ("alert", Level::Critical),
    ("critical", Level::Critical),
    ("crit", Level::Critical),
    ("severe", Level::Critical),
    ("error", Level::Error),
    ("err", Level::Error),
    ("fail", Level::Error),
    ("failed", Level::Error),
    ("warning", Level::Warn),
    ("warn", Level::Warn),
    ("notice", Level::Notice),
    ("note", Level::Notice),
    ("info", Level::Info),
    ("debug", Level::Debug),
    ("dbg", Level::Debug),
    ("trace", Level::Trace),
];

pub fn detect_level(line: &str) -> Level {
    let lower = line.to_ascii_lowercase();
    for (kw, level) in KEYWORDS {
        if has_word(&lower, kw) {
            return *level;
        }
    }
    Level::None
}

fn has_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    for (i, _) in haystack.match_indices(needle) {
        let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let end = i + needle.len();
        let right_ok = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
    }
    false
}

pub fn level_style(l: Level) -> Style {
    match l {
        Level::None => Style::default(),
        Level::Trace => Style::default().fg(Color::DarkGray),
        Level::Debug => Style::default().fg(Color::Blue),
        Level::Info => Style::default().fg(Color::Green),
        Level::Notice => Style::default().fg(Color::Cyan),
        Level::Warn => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        Level::Error => Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
        Level::Critical => Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
        Level::Fatal => Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD),
    }
}

/// Turn raw log text into a vec of styled Lines.
pub fn colorize(text: &str) -> Vec<Line<'static>> {
    text.split('\n')
        .map(|l| {
            let style = level_style(detect_level(l));
            Line::from(Span::styled(l.to_owned(), style))
        })
        .collect()
}

pub fn run(path: PathBuf) -> Result<()> {
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut terminal = ratatui::init();
    let result = App::new(path, body).run(&mut terminal);
    ratatui::restore();
    result
}

struct App {
    path: PathBuf,
    lines: Vec<Line<'static>>,
    scroll: u16,
}

impl App {
    fn new(path: PathBuf, body: String) -> Self {
        Self {
            path,
            lines: colorize(&body),
            scroll: 0,
        }
    }

    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let max = self.max_scroll();
        loop {
            terminal.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => return Ok(()),
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(()),
                (KeyCode::Char('j') | KeyCode::Down, _) => {
                    self.scroll = (self.scroll + 1).min(max);
                }
                (KeyCode::Char('k') | KeyCode::Up, _) => {
                    self.scroll = self.scroll.saturating_sub(1);
                }
                (KeyCode::PageDown, _) => {
                    self.scroll = (self.scroll + 20).min(max);
                }
                (KeyCode::PageUp, _) => {
                    self.scroll = self.scroll.saturating_sub(20);
                }
                (KeyCode::Char('g'), _) => self.scroll = 0,
                (KeyCode::Char('G'), _) => self.scroll = max,
                _ => {}
            }
        }
    }

    fn max_scroll(&self) -> u16 {
        (self.lines.len() as u16).saturating_sub(1)
    }

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);

        let title = format!(" {} ", self.path.display());
        let para = Paragraph::new(self.lines.clone())
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        f.render_widget(para, chunks[0]);

        let status = Line::from(vec![
            Span::styled(
                format!(" {} lines ", self.lines.len()),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw("  j/k scroll  g/G top/bot  q quit"),
        ]);
        f.render_widget(Paragraph::new(status), chunks[1]);
    }
}

