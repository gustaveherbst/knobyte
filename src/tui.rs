//! `knobyte tui`: a modest terminal dashboard (drift, graph, heartbeat, recent events) with
//! views for drift issues, heartbeat details and the timeline, and a quick event log entry.
//!
//! Keys: `1`-`4` or Tab switch views, `r` refresh, `j`/`k` or arrows scroll, `l` log an event
//! (Up/Down choose the kind, Tab switches between message and file, Enter saves), `q` / Esc quit.

use std::io;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::config::KnobyteConfig;
use crate::drift::{inspect_graph, run_drift_check, DriftReport};
use crate::events::{append_logged_event, logging_actor, read_events, EventEntry, EVENT_KINDS};
use crate::heartbeat::{check_heartbeat, configured_stale_days, HeartbeatReport};

pub struct DashboardData {
    pub report: DriftReport,
    pub heartbeat: HeartbeatReport,
    pub graph_status: String,
    pub graph_detail: String,
    pub events: Vec<EventEntry>,
}

pub fn load_dashboard(config: &KnobyteConfig) -> DashboardData {
    let report = run_drift_check(config);
    let heartbeat = check_heartbeat(config, configured_stale_days(config));
    let g = inspect_graph(config);
    let mut events = read_events(config);
    events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    DashboardData {
        report,
        heartbeat,
        graph_status: g.status.as_str().to_string(),
        graph_detail: g.detail.unwrap_or_default(),
        events,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Dashboard,
    Issues,
    Heartbeat,
    Timeline,
}

const VIEWS: [(View, &str); 4] = [
    (View::Dashboard, "1 Dashboard"),
    (View::Issues, "2 Drift issues"),
    (View::Heartbeat, "3 Heartbeat"),
    (View::Timeline, "4 Timeline"),
];

/// The TUI's log entry form: kind, message and an optional related file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogDraft {
    /// Index into [`EVENT_KINDS`].
    pub kind: usize,
    pub message: String,
    pub file: String,
    /// Typing goes to the file field instead of the message.
    pub editing_file: bool,
}

impl LogDraft {
    pub fn new() -> Self {
        let note = EVENT_KINDS.iter().position(|k| *k == "note").unwrap_or(0);
        Self { kind: note, ..Self::default() }
    }

    pub fn kind_name(&self) -> &'static str {
        EVENT_KINDS[self.kind % EVENT_KINDS.len()]
    }

    pub fn next_kind(&mut self) {
        self.kind = (self.kind + 1) % EVENT_KINDS.len();
    }

    pub fn prev_kind(&mut self) {
        self.kind = (self.kind + EVENT_KINDS.len() - 1) % EVENT_KINDS.len();
    }

    fn field(&mut self) -> &mut String {
        if self.editing_file {
            &mut self.file
        } else {
            &mut self.message
        }
    }
}

/// Save a TUI log entry the same way `knobyte log` does (same validation, actor and cwd).
pub fn submit_log(config: &KnobyteConfig, draft: &LogDraft) -> Result<EventEntry, String> {
    let message = draft.message.trim();
    if message.is_empty() {
        return Err("message is empty".to_string());
    }
    let files: Vec<String> = Some(draft.file.trim()).filter(|f| !f.is_empty()).map(str::to_string).into_iter().collect();
    let actor = logging_actor(config);
    append_logged_event(config, message, draft.kind_name(), &[], &files, actor.as_deref(), None, None)
        .map_err(|e| e.to_string())
}

struct App {
    view: View,
    scroll: u16,
    data: DashboardData,
    input: Option<LogDraft>,
    notice: Option<String>,
}

/// Run the dashboard. Returns an error message when the terminal is not interactive.
pub fn run_tui(config: &KnobyteConfig) -> Result<(), String> {
    if !crate::agent::is_interactive() {
        return Err("knobyte tui requires an interactive terminal. Run `knobyte commands` to list CLI commands.".into());
    }
    let mut app = App { view: View::Dashboard, scroll: 0, data: load_dashboard(config), input: None, notice: None };
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, config);
    ratatui::restore();
    result.map_err(|e| e.to_string())
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, config: &KnobyteConfig) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(draft) = app.input.as_mut() {
            match key.code {
                KeyCode::Esc => app.input = None,
                KeyCode::Up => draft.prev_kind(),
                KeyCode::Down => draft.next_kind(),
                KeyCode::Tab | KeyCode::BackTab => draft.editing_file = !draft.editing_file,
                KeyCode::Enter => {
                    let draft = app.input.take().unwrap_or_default();
                    if !draft.message.trim().is_empty() {
                        app.notice = Some(match submit_log(config, &draft) {
                            Ok(e) => format!("Logged {}.", e.kind),
                            Err(e) => format!("Could not log: {}", e),
                        });
                        app.data = load_dashboard(config);
                    }
                }
                KeyCode::Backspace => {
                    draft.field().pop();
                }
                KeyCode::Char(c) => draft.field().push(c),
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('r') => {
                app.data = load_dashboard(config);
                app.notice = Some("Refreshed.".into());
            }
            KeyCode::Char('l') => app.input = Some(LogDraft::new()),
            KeyCode::Char('1') => switch(app, View::Dashboard),
            KeyCode::Char('2') => switch(app, View::Issues),
            KeyCode::Char('3') => switch(app, View::Heartbeat),
            KeyCode::Char('4') => switch(app, View::Timeline),
            KeyCode::Tab => {
                let i = VIEWS.iter().position(|(v, _)| *v == app.view).unwrap_or(0);
                switch(app, VIEWS[(i + 1) % VIEWS.len()].0);
            }
            KeyCode::Down | KeyCode::Char('j') => app.scroll = app.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => app.scroll = app.scroll.saturating_sub(1),
            _ => {}
        }
    }
}

fn switch(app: &mut App, view: View) {
    app.view = view;
    app.scroll = 0;
}

fn score_color(score: f64) -> Color {
    if score >= 90.0 {
        Color::Green
    } else if score >= 70.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5), Constraint::Length(3)])
        .split(f.area());
    let selected = VIEWS.iter().position(|(v, _)| *v == app.view).unwrap_or(0);
    let tabs = Tabs::new(VIEWS.iter().map(|(_, t)| Line::from(*t)).collect::<Vec<_>>())
        .select(selected)
        .block(Block::default().borders(Borders::ALL).title(" Knobyte "))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan));
    f.render_widget(tabs, chunks[0]);

    match app.view {
        View::Dashboard => draw_dashboard(f, app, chunks[1]),
        View::Issues => {
            let lines: Vec<Line> = if app.data.report.issues.is_empty() {
                vec![Line::from("No drift issues.")]
            } else {
                app.data
                    .report
                    .issues
                    .iter()
                    .map(|i| {
                        let color = match i.severity.as_str() {
                            "error" => Color::Red,
                            "warning" => Color::Yellow,
                            _ => Color::Gray,
                        };
                        Line::from(vec![
                            Span::styled(format!("{:<8}", i.severity), Style::default().fg(color)),
                            Span::raw(format!("{} {}: {}", i.code, i.file, i.message)),
                        ])
                    })
                    .collect()
            };
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((app.scroll, 0))
                    .block(Block::default().borders(Borders::ALL).title(" Drift issues (knobyte sync repairs them) ")),
                chunks[1],
            );
        }
        View::Heartbeat => {
            let hb = &app.data.heartbeat;
            let mut lines = vec![Line::from(if hb.heartbeat_ok { "HEARTBEAT_OK".to_string() } else { "Heartbeat needs attention".to_string() })];
            lines.push(Line::from(format!("Status: {}", hb.memory_cleanup_status)));
            lines.push(Line::from(format!("Stale threshold: {} days", hb.stale_days)));
            for s in &hb.stale_files {
                lines.push(Line::from(format!("  stale: {} ({} days, {})", s.path, s.age_days, s.source)));
            }
            if hb.memory_cleanup_due {
                lines.push(Line::from("  memory cleanup is due"));
            }
            for m in &hb.old_daily_memory_files {
                lines.push(Line::from(format!("  old memory file: {}", m)));
            }
            for c in &hb.cleanup_candidates {
                lines.push(Line::from(format!("  temp file: {} ({}; knobyte heartbeat --clean)", c.path, c.reason)));
            }
            f.render_widget(
                Paragraph::new(lines).scroll((app.scroll, 0)).block(Block::default().borders(Borders::ALL).title(" Heartbeat ")),
                chunks[1],
            );
        }
        View::Timeline => {
            let items: Vec<ListItem> = app
                .data
                .events
                .iter()
                .skip(app.scroll as usize)
                .map(|e| ListItem::new(format!("{}  {:<9} {}", e.timestamp.get(..16).unwrap_or(&e.timestamp), e.kind, e.summary)))
                .collect();
            f.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title(" Timeline ")), chunks[1]);
        }
    }

    let footer = if let Some(d) = &app.input {
        let (mc, fc) = if d.editing_file { ("", "_") } else { ("_", "") };
        format!(
            "Log [{}] {}{}  file: {}{}   (Up/Down kind, Tab message/file, Enter save, Esc cancel)",
            d.kind_name(),
            d.message,
            mc,
            d.file,
            fc
        )
    } else {
        format!(
            "q quit  r refresh  Tab/1-4 views  j/k scroll  l log event{}",
            app.notice.as_deref().map(|n| format!("   | {}", n)).unwrap_or_default()
        )
    };
    f.render_widget(Paragraph::new(footer).block(Block::default().borders(Borders::ALL)), chunks[2]);
}

fn draw_dashboard(f: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(6), Constraint::Min(3)])
        .split(area);
    let r = &app.data.report;
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Drift score "))
        .gauge_style(Style::default().fg(score_color(r.score)))
        .percent(r.score.clamp(0.0, 100.0) as u16)
        .label(format!("{:.0}/100  ({} errors, {} warnings, {} files)", r.score, r.count("error"), r.count("warning"), r.file_count));
    f.render_widget(gauge, rows[0]);

    let hb = &app.data.heartbeat;
    let summary = vec![
        Line::from(format!("Graph:     {} {}", app.data.graph_status, app.data.graph_detail)),
        Line::from(format!(
            "Grounding: {}/{} intact ({:.0}%)",
            r.grounding.intact, r.grounding.total, r.grounding_score
        )),
        Line::from(format!(
            "Heartbeat: {}",
            if hb.heartbeat_ok { "HEARTBEAT_OK".to_string() } else { format!("{} stale file(s)", hb.stale_files.len()) }
        )),
        Line::from(format!("Events:    {} logged", app.data.events.len())),
    ];
    f.render_widget(Paragraph::new(summary).block(Block::default().borders(Borders::ALL).title(" Status ")), rows[1]);

    let items: Vec<ListItem> = app
        .data
        .events
        .iter()
        .take(20)
        .map(|e| ListItem::new(format!("{}  {:<9} {}", e.timestamp.get(..10).unwrap_or(&e.timestamp), e.kind, e.summary)))
        .collect();
    f.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title(" Recent events ")), rows[2]);
}
