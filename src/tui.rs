//! Interactive terminal dashboard (ratatui).
//!
//! The dashboard owns the terminal (alternate screen, raw mode, hidden cursor)
//! while it runs and restores it on every exit path. It only reads the shared
//! snapshot and log buffer, and talks to the monitor thread through the command
//! channel, so a slow terminal never blocks monitoring.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};

use crate::config::{Config, Trigger};
use crate::configedit::{ConfigDoc, HostEdit, PppoeDefaults, RciEdit, TargetEdit};
use crate::control::{ControlClient, Request};
use crate::ipclass::IpClass;
use crate::model::{
    Command, HostState, HostStatus, LogLevel, LogLine, SharedLogs, SharedSnapshot, Snapshot,
    WanStatus,
};
use crate::term::RouterBackend;
use crate::util::{format_age, format_hms, format_mac, format_millis, format_uptime};

/// Event poll timeout: keyboard latency on one side, idle CPU use on the other.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The single accent color: product name, keyboard hints.
const ACCENT: Color = Color::Cyan;
/// Neutral borders.
const BORDER: Color = Color::DarkGray;
/// Secondary labels and timestamps.
const MUTED: Color = Color::Gray;
/// Below this the dashboard cannot show all of its parts honestly.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

/// Attach the dashboard to a running daemon through the control socket.
pub fn run_client(socket: &Path, config_path: &Path) -> Result<(), String> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err("the dashboard needs an interactive terminal".to_string());
    }
    install_panic_hook();
    let mut session = TerminalSession::enter().map_err(|err| terminal_error(err).to_string())?;
    let client = ControlClient::connect(socket)
        .map_err(|err| err.to_string())?
        .with_timeout(Duration::from_millis(900));
    let mut app = ClientApp::new(client, config_path.to_path_buf());
    let result = app.event_loop(&mut session.terminal);
    drop(session);
    result.map_err(|err| format!("interactive dashboard failed: {err}"))
}

/// Run the dashboard until the user quits or a stop signal arrives.
pub fn run(
    snapshot: SharedSnapshot,
    logs: SharedLogs,
    commands: Sender<Command>,
) -> io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(not_a_terminal());
    }

    install_panic_hook();
    let mut session = TerminalSession::enter()?;
    let mut ui = UiState::default();
    let result = event_loop(&mut session.terminal, &snapshot, &logs, &commands, &mut ui);
    // Restore the terminal before returning the error to main.
    drop(session);
    result
}

/// Alternate screen + raw mode + hidden cursor, undone on drop.
struct TerminalSession {
    terminal: Terminal<RouterBackend>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode().map_err(raw_mode_error)?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(terminal_error(err));
        }
        match Terminal::new(RouterBackend::new(stdout)) {
            Ok(terminal) => Ok(TerminalSession { terminal }),
            Err(err) => {
                restore_terminal();
                Err(terminal_error(err))
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        restore_terminal();
    }
}

fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

fn not_a_terminal() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "wolguard tui needs an interactive terminal: stdin and stdout must be a TTY. \
         Connect over SSH from a computer (the Keenetic web console has no PTY). \
         For a one-shot status view use `wolguard status`.",
    )
}

fn raw_mode_error(err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!(
            "cannot switch the terminal to raw mode: {err}. \
             Run the dashboard from a real SSH session; \
             for a one-shot status view use `wolguard status`."
        ),
    )
}

fn terminal_error(err: io::Error) -> io::Error {
    io::Error::new(
        err.kind(),
        format!(
            "cannot start the terminal dashboard: {err}. \
             Run it from a real SSH session; \
             for a one-shot status view use `wolguard status`."
        ),
    )
}

/// Restore the terminal before the default hook prints the panic message.
///
/// The release profile uses `panic = "abort"`, so an unwinding `Drop` guard is
/// not enough: without this hook an aborted dashboard leaves the terminal in
/// raw mode on the alternate screen.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn event_loop(
    terminal: &mut Terminal<RouterBackend>,
    snapshot: &SharedSnapshot,
    logs: &SharedLogs,
    commands: &Sender<Command>,
    ui: &mut UiState,
) -> io::Result<()> {
    loop {
        if crate::stop_requested() {
            return Ok(());
        }
        draw(terminal, snapshot, logs, ui)?;
        let Some(event) = poll_event()? else {
            continue;
        };
        // Resize needs no handling of its own: Terminal::draw picks the new
        // size up and repaints fully.
        if let Event::Key(key) = event {
            if key.kind != KeyEventKind::Release && handle_key(key, ui, commands, snapshot) {
                return Ok(());
            }
        }
    }
}

fn poll_event() -> io::Result<Option<Event>> {
    match event::poll(POLL_INTERVAL) {
        Ok(false) => Ok(None),
        Ok(true) => match event::read() {
            Ok(event) => Ok(Some(event)),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
            Err(err) => Err(err),
        },
        // A signal interrupts the wait; the loop top re-checks stop_requested().
        Err(err) if err.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(err) => Err(err),
    }
}

/// Handle one key press; returns true when the dashboard should close.
fn handle_key(
    key: KeyEvent,
    ui: &mut UiState,
    commands: &Sender<Command>,
    snapshot: &SharedSnapshot,
) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up | KeyCode::Char('k') => ui.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => ui.select_next(),
        KeyCode::Char('w') => {
            if let Some(index) = ui.selected_index() {
                let name = snapshot
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .hosts
                    .get(index)
                    .map(|host| host.name.clone());
                if let Some(name) = name {
                    // The monitor may already be gone; the dashboard keeps
                    // showing the last snapshot either way.
                    let _ = commands.send(Command::WakeNow(name));
                }
            }
        }
        KeyCode::Char('e') => {
            if let Some(index) = ui.selected_index() {
                let _ = commands.send(Command::ToggleEnabled(index));
            }
        }
        _ => {}
    }
    false
}

#[derive(Clone, Copy, Default)]
struct UiState {
    selected: usize,
    host_count: usize,
}

impl UiState {
    fn update_hosts(&mut self, host_count: usize) {
        self.host_count = host_count;
        self.selected = self.selected_index().unwrap_or(0);
    }

    fn selected_index(&self) -> Option<usize> {
        if self.host_count == 0 {
            None
        } else {
            Some(self.selected.min(self.host_count - 1))
        }
    }

    fn select_previous(&mut self) {
        if let Some(index) = self.selected_index() {
            self.selected = index.saturating_sub(1);
        }
    }

    fn select_next(&mut self) {
        if let Some(index) = self.selected_index() {
            self.selected = (index + 1).min(self.host_count - 1);
        }
    }
}

fn draw(
    terminal: &mut Terminal<RouterBackend>,
    snapshot: &SharedSnapshot,
    logs: &SharedLogs,
    ui: &mut UiState,
) -> io::Result<()> {
    let size = terminal.size()?;
    let terminal_area = Rect::new(0, 0, size.width, size.height);
    let log_rows = layout_areas(terminal_area).log.height.saturating_sub(2) as usize;

    // Copy the data the frame needs under short locks and render afterwards, so
    // drawing can never delay the monitor thread.
    let snapshot = snapshot
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let log_lines = recent_logs(logs, log_rows.max(1));

    let now = SystemTime::now();
    ui.update_hosts(snapshot.hosts.len());
    terminal.draw(|frame| render(frame, &snapshot, &log_lines, *ui, now))?;
    Ok(())
}

fn recent_logs(logs: &SharedLogs, rows: usize) -> Vec<LogLine> {
    let buffer = logs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let skip = buffer.len().saturating_sub(rows);
    buffer.iter().skip(skip).cloned().collect()
}

#[derive(Clone, Copy)]
struct Areas {
    header: Rect,
    table: Rect,
    log: Rect,
    footer: Rect,
}

/// Fixed-position layout with saturating arithmetic: any terminal size gives
/// non-overlapping rectangles that fit inside the frame.
fn layout_areas(area: Rect) -> Areas {
    let footer_height = u16::from(area.height >= 3);
    let header_height = if area.height >= 8 {
        2
    } else {
        u16::from(area.height >= 5)
    };
    let body_height = area.height.saturating_sub(header_height + footer_height);
    let log_height = log_height(area.height, body_height);
    let table_height = body_height.saturating_sub(log_height);
    let table_y = area.y.saturating_add(header_height);
    let log_y = table_y.saturating_add(table_height);
    let footer_y = area
        .y
        .saturating_add(area.height)
        .saturating_sub(footer_height);
    Areas {
        header: Rect::new(area.x, area.y, area.width, header_height),
        table: Rect::new(area.x, table_y, area.width, table_height),
        log: Rect::new(area.x, log_y, area.width, log_height),
        footer: Rect::new(area.x, footer_y, area.width, footer_height),
    }
}

/// The log panel takes about 40% of the terminal, but never grows so far that
/// the table loses its header row or shrinks below three usable rows.
fn log_height(area_height: u16, body_height: u16) -> u16 {
    if body_height < 7 {
        return 0;
    }
    let wanted = (u32::from(area_height) * 2 / 5) as u16;
    wanted.clamp(3, body_height - 4)
}

fn render(frame: &mut Frame, snapshot: &Snapshot, logs: &[LogLine], ui: UiState, now: SystemTime) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }

    let areas = layout_areas(area);
    render_header(frame, areas.header, snapshot, now);
    render_hosts(frame, areas.table, snapshot, ui, now);
    if !areas.log.is_empty() {
        render_logs(frame, areas.log, logs);
    }
    render_footer(frame, areas.footer);
}

fn render_too_small(frame: &mut Frame, area: Rect) {
    let message = format!(
        "terminal too small: wolguard needs at least {MIN_WIDTH}x{MIN_HEIGHT}, this is {}x{}",
        area.width, area.height
    );
    frame.render_widget(Paragraph::new(message).style(label_style()), area);
}

fn render_header(frame: &mut Frame, area: Rect, snapshot: &Snapshot, now: SystemTime) {
    let title = Line::from(vec![
        Span::styled(
            format!("wolguard v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("config ", label_style()),
        Span::raw(snapshot.config_path.as_str()),
        Span::raw("  "),
        Span::styled("uptime ", label_style()),
        Span::raw(format_uptime(snapshot.started_at, now)),
    ]);
    let targets = Line::from(vec![
        Span::styled("wake target ", label_style()),
        Span::raw(snapshot.wake_target.as_str()),
        Span::raw("  "),
        Span::styled("udp ", label_style()),
        Span::raw(snapshot.wol_port.to_string()),
    ]);
    let line_count = usize::from(area.height.min(2));
    let lines: Vec<Line> = [title, targets].into_iter().take(line_count).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_hosts(frame: &mut Frame, area: Rect, snapshot: &Snapshot, ui: UiState, now: SystemTime) {
    let columns = columns(area.width);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" hosts ({}) ", snapshot.hosts.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);

    let widths: Vec<Constraint> = columns.iter().map(|column| column.constraint()).collect();
    let header = Row::new(columns.iter().map(|column| Cell::from(column.label())));
    let rows = snapshot.hosts.iter().enumerate().map(|(index, host)| {
        Row::new(
            columns
                .iter()
                .map(|column| column.cell(index, host, now))
                .collect::<Vec<Cell>>(),
        )
    });
    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut table_state = TableState::default().with_selected(ui.selected_index());
    frame.render_stateful_widget(table, area, &mut table_state);

    if snapshot.hosts.is_empty() && inner.height > 1 {
        let message = format!(
            "no hosts configured; add [[host]] entries to {} and restart",
            snapshot.config_path
        );
        let message_area = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        frame.render_widget(Paragraph::new(message).style(label_style()), message_area);
    }
}

fn render_logs(frame: &mut Frame, area: Rect, logs: &[LogLine]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            " log ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    if logs.is_empty() {
        frame.render_widget(
            Paragraph::new("no log lines yet").style(label_style()),
            inner,
        );
        return;
    }

    // Auto-scroll: only the newest lines that fit are rendered.
    let rows = inner.height as usize;
    let visible = &logs[logs.len().saturating_sub(rows)..];
    let lines: Vec<Line> = visible
        .iter()
        .map(|line| {
            Line::from(vec![
                Span::styled(format_hms(line.at), label_style()),
                Span::raw(" "),
                Span::styled(line.level.label(), level_style(line.level)),
                Span::raw(" "),
                Span::raw(line.text.as_str()),
            ])
        })
        .collect();
    // No wrapping: lines that do not fit are clipped, which keeps one log entry
    // on one row and can never split a multi-byte character.
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_footer(frame: &mut Frame, area: Rect) {
    let keys = Line::from(vec![
        Span::styled("q/Esc", accent_style()),
        Span::raw(" quit   "),
        Span::styled("j/k", accent_style()),
        Span::raw(" up/down   "),
        Span::styled("w", accent_style()),
        Span::raw(" wake   "),
        Span::styled("e", accent_style()),
        Span::raw(" enable/disable"),
    ]);
    frame.render_widget(Paragraph::new(keys), area);
}

/// One host table column.
///
/// Columns after the name are dropped on narrow terminals instead of being
/// squeezed below their readable width; the name takes the leftover space.
#[derive(Clone, Copy)]
enum Column {
    Index,
    Name,
    Ip,
    Mac,
    State,
    Rtt,
    LastSeen,
    Wakes,
    LastWake,
}

impl Column {
    fn label(self) -> &'static str {
        match self {
            Column::Index => "#",
            Column::Name => "Name",
            Column::Ip => "IP",
            Column::Mac => "MAC",
            Column::State => "State",
            Column::Rtt => "RTT",
            Column::LastSeen => "Last seen",
            Column::Wakes => "Wakes",
            Column::LastWake => "Last wake",
        }
    }

    fn constraint(self) -> Constraint {
        match self {
            Column::Name => Constraint::Min(8),
            Column::Index => Constraint::Length(3),
            Column::Ip => Constraint::Length(15),
            Column::Mac => Constraint::Length(17),
            Column::State => Constraint::Length(8),
            Column::Rtt => Constraint::Length(8),
            Column::LastSeen => Constraint::Length(10),
            Column::Wakes => Constraint::Length(6),
            Column::LastWake => Constraint::Length(10),
        }
    }

    fn cell(self, index: usize, host: &HostStatus, now: SystemTime) -> Cell<'static> {
        match self {
            Column::Index => {
                Cell::from(Text::from((index + 1).to_string()).alignment(Alignment::Right))
            }
            Column::Name => Cell::from(host.name.clone()),
            Column::Ip => Cell::from(host.ip.to_string()),
            Column::Mac => Cell::from(format_mac(host.mac)),
            Column::State => Cell::from(host.state.label()).style(state_style(host.state)),
            Column::Rtt => Cell::from(Text::from(format_rtt(host)).alignment(Alignment::Right)),
            Column::LastSeen => Cell::from(format_age(now, host.last_ok)),
            Column::Wakes => {
                Cell::from(Text::from(host.wake_count.to_string()).alignment(Alignment::Right))
            }
            Column::LastWake => Cell::from(format_age(now, host.last_wake)),
        }
    }
}

/// Column layout as a function of the terminal width.
///
/// The thresholds are the smallest widths at which a column still fits next to
/// the other visible ones, including one cell of spacing and two borders. The
/// canonical column order is kept for every subset.
fn columns(width: u16) -> Vec<Column> {
    let mut columns = Vec::with_capacity(9);
    columns.push(Column::Index);
    columns.push(Column::Name);
    columns.push(Column::Ip);
    if width >= 77 {
        columns.push(Column::Mac);
    }
    columns.push(Column::State);
    if width >= 48 {
        columns.push(Column::Rtt);
    }
    if width >= 59 {
        columns.push(Column::LastSeen);
    }
    if width >= 84 {
        columns.push(Column::Wakes);
    }
    if width >= 95 {
        columns.push(Column::LastWake);
    }
    columns
}

fn format_rtt(host: &HostStatus) -> String {
    host.last_rtt.map_or_else(|| "-".to_string(), format_millis)
}

fn label_style() -> Style {
    Style::default().fg(MUTED)
}

fn accent_style() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

fn state_style(state: HostState) -> Style {
    let color = match state {
        HostState::Online => Color::Green,
        HostState::Offline => Color::Red,
        HostState::Waking => Color::Yellow,
        HostState::Unknown => Color::Gray,
        HostState::Disabled => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn level_style(level: LogLevel) -> Style {
    match level {
        LogLevel::Info => Style::default(),
        LogLevel::Warn => Style::default().fg(Color::Yellow),
        LogLevel::Error => Style::default().fg(Color::Red),
    }
}

// ---------------------------------------------------------------------------
// Management console (client of the daemon control socket)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClientTab {
    Overview,
    Hosts,
    Wan,
    Settings,
    Logs,
}

impl ClientTab {
    const ALL: [ClientTab; 5] = [
        ClientTab::Overview,
        ClientTab::Hosts,
        ClientTab::Wan,
        ClientTab::Settings,
        ClientTab::Logs,
    ];

    fn index(self) -> usize {
        match self {
            ClientTab::Overview => 0,
            ClientTab::Hosts => 1,
            ClientTab::Wan => 2,
            ClientTab::Settings => 3,
            ClientTab::Logs => 4,
        }
    }

    fn title(self) -> &'static str {
        match self {
            ClientTab::Overview => "Overview",
            ClientTab::Hosts => "Hosts",
            ClientTab::Wan => "WAN / PPPoE",
            ClientTab::Settings => "Settings",
            ClientTab::Logs => "Logs",
        }
    }
}

enum Modal {
    Confirm {
        title: String,
        question: String,
        action: ConfirmAction,
    },
    Form(Form),
}

enum ConfirmAction {
    RestartWan(String),
    TestWan(String),
    RemoveWan(String),
    RemoveHost(String),
    StartService,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Bool,
    Int,
    Trigger,
    List,
}

struct FormField {
    label: String,
    value: String,
    kind: FieldKind,
}

struct Form {
    title: String,
    action: FormAction,
    fields: Vec<FormField>,
    selected: usize,
    error: Option<String>,
}

enum FormAction {
    AddHost,
    EditHost(String),
    WanRules(String),
    Settings,
}

impl Form {
    fn field(&self, label: &str) -> Option<&FormField> {
        self.fields.iter().find(|field| field.label == label)
    }

    fn value(&self, label: &str) -> &str {
        self.field(label)
            .map(|field| field.value.as_str())
            .unwrap_or("")
    }

    fn current_mut(&mut self) -> &mut FormField {
        &mut self.fields[self.selected]
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.fields.len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.current_mut().value.pop();
            }
            KeyCode::Enter => {
                let kind = self.current_mut().kind;
                if matches!(kind, FieldKind::Bool | FieldKind::Trigger) {
                    self.toggle_current();
                } else {
                    self.selected = (self.selected + 1) % self.fields.len().max(1);
                }
            }
            KeyCode::Char(' ') => {
                let kind = self.current_mut().kind;
                if matches!(kind, FieldKind::Bool | FieldKind::Trigger) {
                    self.toggle_current();
                } else {
                    self.current_mut().value.push(' ');
                }
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_graphic() =>
            {
                self.current_mut().value.push(ch);
            }
            _ => {}
        }
    }

    fn toggle_current(&mut self) {
        let field = self.current_mut();
        match field.kind {
            FieldKind::Bool => {
                field.value = if field.value.eq_ignore_ascii_case("true") {
                    "false".to_string()
                } else {
                    "true".to_string()
                };
            }
            FieldKind::Trigger => {
                field.value = if field.value == "always" {
                    "on-change".to_string()
                } else {
                    "always".to_string()
                };
            }
            _ => {}
        }
    }

    fn host_form(existing: Option<&crate::config::HostConfig>, title: &str) -> Form {
        let value = |text: Option<String>| text.unwrap_or_default();
        let fields = vec![
            FormField {
                label: "name".to_string(),
                value: existing.map(|host| host.name.clone()).unwrap_or_default(),
                kind: FieldKind::Text,
            },
            FormField {
                label: "ip".to_string(),
                value: existing.map(|host| host.ip.to_string()).unwrap_or_default(),
                kind: FieldKind::Text,
            },
            FormField {
                label: "mac".to_string(),
                value: existing.map(|host| host.mac.clone()).unwrap_or_default(),
                kind: FieldKind::Text,
            },
            FormField {
                label: "enabled".to_string(),
                value: existing
                    .map(|host| host.enabled.to_string())
                    .unwrap_or_else(|| "true".to_string()),
                kind: FieldKind::Bool,
            },
            FormField {
                label: "check_interval_secs".to_string(),
                value: value(
                    existing
                        .and_then(|host| host.check_interval_secs)
                        .map(|v| v.to_string()),
                ),
                kind: FieldKind::Int,
            },
            FormField {
                label: "wake_retry_secs".to_string(),
                value: value(
                    existing
                        .and_then(|host| host.wake_retry_secs)
                        .map(|v| v.to_string()),
                ),
                kind: FieldKind::Int,
            },
            FormField {
                label: "failures_before_wake".to_string(),
                value: value(
                    existing
                        .and_then(|host| host.failures_before_wake)
                        .map(|v| v.to_string()),
                ),
                kind: FieldKind::Int,
            },
            FormField {
                label: "max_wakes_per_hour".to_string(),
                value: value(
                    existing
                        .and_then(|host| host.max_wakes_per_hour)
                        .map(|v| v.to_string()),
                ),
                kind: FieldKind::Int,
            },
        ];
        Form {
            title: title.to_string(),
            action: existing
                .map(|host| FormAction::EditHost(host.name.clone()))
                .unwrap_or(FormAction::AddHost),
            fields,
            selected: 0,
            error: None,
        }
    }

    fn rules_form(target: &crate::config::PppoeTargetSettings, title: &str) -> Form {
        Form {
            title: title.to_string(),
            action: FormAction::WanRules(target.interface.clone()),
            fields: vec![
                FormField {
                    label: "trigger".to_string(),
                    value: target.trigger.label().to_string(),
                    kind: FieldKind::Trigger,
                },
                FormField {
                    label: "poll_secs".to_string(),
                    value: target.poll.as_secs().to_string(),
                    kind: FieldKind::Int,
                },
                FormField {
                    label: "down_secs".to_string(),
                    value: target.down.as_secs().to_string(),
                    kind: FieldKind::Int,
                },
                FormField {
                    label: "settle_secs".to_string(),
                    value: target.settle.as_secs().to_string(),
                    kind: FieldKind::Int,
                },
                FormField {
                    label: "retry_secs".to_string(),
                    value: target.retry.as_secs().to_string(),
                    kind: FieldKind::Int,
                },
                FormField {
                    label: "max_restarts_per_hour".to_string(),
                    value: target.max_restarts_per_hour.to_string(),
                    kind: FieldKind::Int,
                },
                FormField {
                    label: "white_cidrs".to_string(),
                    value: target.white_cidrs.join(","),
                    kind: FieldKind::List,
                },
                FormField {
                    label: "grey_cidrs".to_string(),
                    value: target.grey_cidrs.join(","),
                    kind: FieldKind::List,
                },
            ],
            selected: 0,
            error: None,
        }
    }

    fn settings_form(config: &Config) -> Form {
        Form {
            title: "Settings".to_string(),
            action: FormAction::Settings,
            fields: vec![
                bool_field("wol_enabled", config.wol_enabled),
                int_field("check_interval_secs", config.check_interval_secs),
                int_field("ping_timeout_ms", config.ping_timeout_ms),
                int_field("failures_before_wake", config.failures_before_wake as u64),
                int_field("wake_retry_secs", config.wake_retry_secs),
                int_field("max_wakes_per_hour", config.max_wakes_per_hour as u64),
                bool_field("wake_on_start", config.wake_on_start),
                bool_field("verbose_probes", config.verbose_probes),
                text_field("rci.url", config.rci.url.clone()),
                text_field("rci.token", config.rci.token.clone()),
                int_field("rci.timeout_ms", config.rci.timeout_ms),
                bool_field("pppoe.enabled", config.pppoe.enabled),
                FormField {
                    label: "pppoe.trigger".to_string(),
                    value: config.pppoe.trigger.label().to_string(),
                    kind: FieldKind::Trigger,
                },
                int_field("pppoe.poll_secs", config.pppoe.poll_secs),
                int_field("pppoe.down_secs", config.pppoe.down_secs),
                int_field("pppoe.settle_secs", config.pppoe.settle_secs),
                int_field("pppoe.retry_secs", config.pppoe.retry_secs),
                int_field(
                    "pppoe.max_restarts_per_hour",
                    config.pppoe.max_restarts_per_hour as u64,
                ),
                FormField {
                    label: "pppoe.white_cidrs".to_string(),
                    value: config.pppoe.white_cidrs.join(","),
                    kind: FieldKind::List,
                },
                FormField {
                    label: "pppoe.grey_cidrs".to_string(),
                    value: config.pppoe.grey_cidrs.join(","),
                    kind: FieldKind::List,
                },
                int_field("wol_port", config.wol_port as u64),
                text_field(
                    "broadcast",
                    config
                        .broadcast
                        .clone()
                        .unwrap_or_else(|| "auto".to_string()),
                ),
                text_field("log_file", config.log_file.clone()),
                text_field("control_socket", config.control_socket.clone()),
                text_field("init_script", config.init_script.clone()),
                text_field("state_file", config.state_file.clone()),
                bool_field("rci.auto_token", config.rci.auto_token),
                text_field("rci.ndmc_path", config.rci.ndmc_path.clone()),
            ],
            selected: 0,
            error: None,
        }
    }
}

fn text_field(label: &str, value: String) -> FormField {
    FormField {
        label: label.to_string(),
        value,
        kind: FieldKind::Text,
    }
}

fn int_field(label: &str, value: u64) -> FormField {
    FormField {
        label: label.to_string(),
        value: value.to_string(),
        kind: FieldKind::Int,
    }
}

fn bool_field(label: &str, value: bool) -> FormField {
    FormField {
        label: label.to_string(),
        value: value.to_string(),
        kind: FieldKind::Bool,
    }
}

struct ClientApp {
    client: ControlClient,
    config_path: PathBuf,
    config: Option<Config>,
    snapshot: Option<Snapshot>,
    logs: Vec<LogLine>,
    last_log_seq: u64,
    connected: bool,
    daemon_pid: Option<u64>,
    path_warned: bool,
    tab: ClientTab,
    host_selected: usize,
    wan_selected: usize,
    log_offset: usize,
    log_follow: bool,
    log_rows: usize,
    log_filter: Option<LogLevel>,
    modal: Option<Modal>,
    message: Option<(String, bool)>,
    last_status: Instant,
    last_logs: Instant,
}

impl ClientApp {
    fn new(client: ControlClient, config_path: PathBuf) -> ClientApp {
        let config = Config::load(&config_path).ok();
        ClientApp {
            client,
            config_path,
            config,
            snapshot: None,
            logs: Vec::new(),
            last_log_seq: 0,
            connected: false,
            daemon_pid: None,
            path_warned: false,
            tab: ClientTab::Overview,
            host_selected: 0,
            wan_selected: 0,
            log_offset: 0,
            log_follow: true,
            log_rows: 10,
            log_filter: None,
            modal: None,
            message: None,
            last_status: Instant::now() - Duration::from_secs(5),
            last_logs: Instant::now() - Duration::from_secs(5),
        }
    }

    fn event_loop(&mut self, terminal: &mut Terminal<RouterBackend>) -> io::Result<()> {
        loop {
            if crate::stop_requested() {
                return Ok(());
            }
            self.poll();
            let size = terminal.size()?;
            self.log_rows = size.height.saturating_sub(6) as usize;
            let area = Rect::new(0, 0, size.width, size.height);
            let snapshot = self.snapshot.clone();
            let config = self.config.clone();
            let logs = self.logs.clone();
            let connected = self.connected;
            let daemon_pid = self.daemon_pid;
            let tab = self.tab;
            let host_selected = self.host_selected;
            let wan_selected = self.wan_selected;
            let log_offset = self.log_offset;
            let log_follow = self.log_follow;
            let log_filter = self.log_filter;
            let message = self.message.clone();
            let modal = self.modal.as_ref();
            terminal.draw(|frame| {
                render_client(
                    frame,
                    area,
                    snapshot.as_ref(),
                    config.as_ref(),
                    &logs,
                    connected,
                    daemon_pid,
                    tab,
                    host_selected,
                    wan_selected,
                    log_offset,
                    log_follow,
                    log_filter,
                    message.as_ref(),
                    modal,
                    SystemTime::now(),
                );
            })?;
            let Some(event) = poll_event()? else {
                continue;
            };
            if let Event::Key(key) = event {
                if key.kind != KeyEventKind::Release && self.handle_key(key) {
                    return Ok(());
                }
            }
        }
    }

    fn command_client(&self) -> ControlClient {
        // Keep the UI responsive: a command may wait for the WAN thread's
        // in-flight RCI call, but it must not block the dashboard for the
        // full CLI timeout. A timed-out command can still complete in the
        // daemon; `s` refreshes the state.
        self.client.clone().with_timeout(Duration::from_secs(8))
    }

    fn poll(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_status) >= Duration::from_secs(1) {
            self.last_status = now;
            match self.client.status() {
                Ok(data) => {
                    self.connected = true;
                    let new_pid = data.get("daemon_pid").and_then(|value| value.as_u64());
                    if self.daemon_pid != new_pid {
                        self.logs.clear();
                        self.last_log_seq = 0;
                    }
                    self.daemon_pid = new_pid;
                    if let Some(daemon_path) =
                        data.get("config_path").and_then(|value| value.as_str())
                    {
                        if Path::new(daemon_path) != self.config_path.as_path() && !self.path_warned
                        {
                            self.path_warned = true;
                            self.message = Some((
                                format!(
                                    "daemon uses {daemon_path}, not {}",
                                    self.config_path.display()
                                ),
                                true,
                            ));
                        }
                    }
                    if let Some(last) = data.get("last_log_seq").and_then(|value| value.as_u64()) {
                        if last < self.last_log_seq {
                            self.logs.clear();
                            self.last_log_seq = 0;
                        }
                    }
                    if let Some(value) = data.get("snapshot") {
                        if let Ok(snapshot) = serde_json::from_value::<Snapshot>(value.clone()) {
                            self.host_selected = self
                                .host_selected
                                .min(snapshot.hosts.len().saturating_sub(1));
                            self.wan_selected =
                                self.wan_selected.min(snapshot.wan.len().saturating_sub(1));
                            self.snapshot = Some(snapshot);
                        }
                    }
                }
                Err(err) => {
                    self.connected = false;
                    if self.message.is_none() {
                        self.message = Some((err, true));
                    }
                }
            }
        }

        if self.connected && now.duration_since(self.last_logs) >= Duration::from_secs(1) {
            self.last_logs = now;
            if let Ok(response) = self.client.request(&Request::with_args(
                "logs",
                serde_json::json!({"after": self.last_log_seq, "limit": 250}),
            )) {
                if response.ok {
                    if let Some(lines) = response
                        .data
                        .as_ref()
                        .and_then(|data| data.get("lines"))
                        .and_then(|value| value.as_array())
                    {
                        for line in lines {
                            if let Ok(line) = serde_json::from_value::<LogLine>(line.clone()) {
                                self.last_log_seq = self.last_log_seq.max(line.seq);
                                self.logs.push(line);
                            }
                        }
                        if self.logs.len() > 2000 {
                            let excess = self.logs.len() - 2000;
                            self.logs.drain(..excess);
                        }
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.modal.is_some() {
            self.handle_modal_key(key);
            return false;
        }
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => return true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Tab => {
                self.tab = next_tab(self.tab, 1);
                self.message = None;
            }
            KeyCode::BackTab => {
                self.tab = next_tab(self.tab, -1);
                self.message = None;
            }
            KeyCode::Char('1') => self.tab = ClientTab::Overview,
            KeyCode::Char('2') => self.tab = ClientTab::Hosts,
            KeyCode::Char('3') => self.tab = ClientTab::Wan,
            KeyCode::Char('4') => self.tab = ClientTab::Settings,
            KeyCode::Char('5') => self.tab = ClientTab::Logs,
            KeyCode::Char('R') => {
                self.last_status = Instant::now() - Duration::from_secs(5);
                self.message = Some(("reconnecting...".to_string(), false));
            }
            KeyCode::Char('S') => {
                self.modal = Some(Modal::Confirm {
                    title: "Service".to_string(),
                    question: "Start the wolguard service now?".to_string(),
                    action: ConfirmAction::StartService,
                });
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('w') => self.action_wake_or_watch(),
            KeyCode::Char(' ') => self.action_toggle_host(),
            KeyCode::Char('a') => self.open_host_form(None),
            KeyCode::Char('e') => self.action_edit(),
            KeyCode::Char('d') => self.action_remove(),
            KeyCode::Char('r') => self.action_restart(false),
            KeyCode::Char('t') => self.action_restart(true),
            KeyCode::Char('s') => {
                if self.tab == ClientTab::Wan {
                    match self.command_client().request(&Request::new("pppoe-scan")) {
                        Ok(response) if response.ok => {
                            if let Some(interfaces) = response
                                .data
                                .as_ref()
                                .and_then(|data| data.get("interfaces"))
                                .and_then(|value| value.as_array())
                            {
                                let statuses: Vec<WanStatus> = interfaces
                                    .iter()
                                    .filter_map(|value| {
                                        serde_json::from_value::<WanStatus>(value.clone()).ok()
                                    })
                                    .collect();
                                if let Some(snapshot) = self.snapshot.as_mut() {
                                    snapshot.wan = statuses;
                                }
                            }
                            self.message = Some(("PPPoE rescanned".to_string(), false));
                        }
                        Ok(response) => {
                            self.message = Some((
                                response
                                    .error
                                    .unwrap_or_else(|| "rescan failed".to_string()),
                                true,
                            ))
                        }
                        Err(err) => self.message = Some((err, true)),
                    }
                } else {
                    self.last_status = Instant::now() - Duration::from_secs(5);
                    self.last_logs = Instant::now() - Duration::from_secs(5);
                    self.message = Some(("refreshing...".to_string(), false));
                }
            }
            KeyCode::Char('f') => self.log_follow = !self.log_follow,
            KeyCode::Char('g') => {
                self.log_follow = false;
                self.log_offset = 0;
            }
            KeyCode::Char('G') => {
                self.log_follow = true;
                self.log_offset = 0;
            }
            KeyCode::Char('i') => {
                self.log_filter = toggle_log_filter(self.log_filter, LogLevel::Info)
            }
            KeyCode::Char('W') => {
                self.log_filter = toggle_log_filter(self.log_filter, LogLevel::Warn)
            }
            KeyCode::Char('E') => {
                self.log_filter = toggle_log_filter(self.log_filter, LogLevel::Error)
            }
            KeyCode::Char('A') => self.log_filter = None,
            _ => {}
        }
        false
    }

    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            ClientTab::Overview | ClientTab::Hosts => {
                let count = self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.hosts.len())
                    .unwrap_or(0);
                self.host_selected = move_index(self.host_selected, count, delta);
            }
            ClientTab::Wan => {
                let count = self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.wan.len())
                    .unwrap_or(0);
                self.wan_selected = move_index(self.wan_selected, count, delta);
            }
            ClientTab::Logs => {
                let total = self.filtered_log_count();
                let max_start = total.saturating_sub(self.log_rows.max(1));
                if self.log_follow {
                    self.log_follow = false;
                    self.log_offset = max_start;
                }
                if delta > 0 {
                    self.log_offset = self.log_offset.saturating_add(1).min(max_start);
                } else {
                    self.log_offset = self.log_offset.saturating_sub(1);
                }
            }
            ClientTab::Settings => {}
        }
    }

    fn selected_host(&self) -> Option<&HostStatus> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.hosts.get(self.host_selected))
    }

    fn selected_wan(&self) -> Option<&WanStatus> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.wan.get(self.wan_selected))
    }

    fn action_wake_or_watch(&mut self) {
        match self.tab {
            ClientTab::Overview | ClientTab::Hosts => {
                if let Some(host) = self.selected_host() {
                    let name = host.name.clone();
                    match self.command_client().request(&Request::with_args(
                        "wake",
                        serde_json::json!({"host": name}),
                    )) {
                        Ok(response) if response.ok => {
                            self.message = Some((format!("wake queued for {name}"), false))
                        }
                        Ok(response) => {
                            self.message = Some((
                                response.error.unwrap_or_else(|| "wake failed".to_string()),
                                true,
                            ))
                        }
                        Err(err) => self.message = Some((err, true)),
                    }
                }
            }
            ClientTab::Wan => self.toggle_watch(),
            _ => {}
        }
    }

    fn action_toggle_host(&mut self) {
        if !matches!(self.tab, ClientTab::Overview | ClientTab::Hosts) {
            return;
        }
        let Some(host) = self.selected_host().cloned() else {
            return;
        };
        let Ok(mut doc) = ConfigDoc::load(&self.config_path) else {
            return;
        };
        if let Err(err) = doc.set_host_enabled(&host.name, !host.enabled) {
            self.message = Some((err, true));
            return;
        }
        self.commit_doc(doc, format!("{} monitoring toggled", host.name));
    }

    fn action_edit(&mut self) {
        match self.tab {
            ClientTab::Overview | ClientTab::Hosts => {
                if let Some(host) = self.selected_host() {
                    let name = host.name.clone();
                    let config = Config::load(&self.config_path).ok();
                    let existing = config
                        .as_ref()
                        .and_then(|config| config.hosts.iter().find(|entry| entry.name == name))
                        .cloned();
                    self.open_host_form(existing);
                }
            }
            ClientTab::Wan => {
                if let Some(interface) = self.selected_wan().map(|wan| wan.id.clone()) {
                    self.open_rules_form(&interface);
                }
            }
            ClientTab::Settings => {
                if let Ok(config) = Config::load(&self.config_path) {
                    self.modal = Some(Modal::Form(Form::settings_form(&config)));
                }
            }
            _ => {}
        }
    }

    fn action_remove(&mut self) {
        match self.tab {
            ClientTab::Overview | ClientTab::Hosts => {
                if let Some(host) = self.selected_host() {
                    self.modal = Some(Modal::Confirm {
                        title: "Remove host".to_string(),
                        question: format!("Remove host {}?", host.name),
                        action: ConfirmAction::RemoveHost(host.name.clone()),
                    });
                }
            }
            ClientTab::Wan => {
                if let Some(wan) = self.selected_wan() {
                    if wan.watched {
                        self.modal = Some(Modal::Confirm {
                            title: "Remove PPPoE target".to_string(),
                            question: format!(
                                "Remove target {} and its rules? \
                                 Use `w` to only disable it.",
                                wan.id
                            ),
                            action: ConfirmAction::RemoveWan(wan.id.clone()),
                        });
                    } else {
                        self.message = Some((
                            "this interface is not a configured target".to_string(),
                            false,
                        ));
                    }
                }
            }
            _ => {}
        }
    }

    fn action_restart(&mut self, test: bool) {
        if self.tab != ClientTab::Wan {
            return;
        }
        if let Some(wan) = self.selected_wan() {
            let interface = wan.id.clone();
            self.modal = Some(Modal::Confirm {
                title: if test {
                    "Test PPPoE".to_string()
                } else {
                    "Restart PPPoE".to_string()
                },
                question: format!(
                    "{} {}? The active WAN drops and SSH may disconnect; the daemon stays up.",
                    if test {
                        "Run one test cycle on"
                    } else {
                        "Restart"
                    },
                    interface
                ),
                action: if test {
                    ConfirmAction::TestWan(interface)
                } else {
                    ConfirmAction::RestartWan(interface)
                },
            });
        }
    }

    fn toggle_watch(&mut self) {
        let Some(interface) = self.selected_wan().map(|wan| wan.id.clone()) else {
            return;
        };
        let Ok(config) = Config::load(&self.config_path) else {
            return;
        };
        let target = config
            .pppoe
            .targets
            .iter()
            .find(|target| target.interface == interface)
            .cloned();
        let Ok(mut doc) = ConfigDoc::load(&self.config_path) else {
            return;
        };
        let result = match target {
            Some(target) => doc.set_target_enabled(&interface, !target.enabled),
            None => doc.watch_target(&interface),
        };
        match result {
            Ok(()) => self.commit_doc(doc, format!("{interface} watch setting saved")),
            Err(err) => self.message = Some((err, true)),
        }
    }

    fn open_host_form(&mut self, existing: Option<crate::config::HostConfig>) {
        let title = if existing.is_some() {
            "Edit host"
        } else {
            "Add host"
        };
        self.modal = Some(Modal::Form(Form::host_form(existing.as_ref(), title)));
    }

    fn open_rules_form(&mut self, interface: &str) {
        let Ok(config) = Config::load(&self.config_path) else {
            return;
        };
        let Ok(targets) = config.pppoe_targets() else {
            return;
        };
        if let Some(target) = targets.iter().find(|target| target.interface == interface) {
            self.modal = Some(Modal::Form(Form::rules_form(
                target,
                &format!("Rules for {interface}"),
            )));
        } else {
            self.message = Some((
                format!("{interface} is not a target; press w to watch it first"),
                false,
            ));
        }
    }

    fn commit_doc(&mut self, mut doc: ConfigDoc, message: String) {
        match doc.save(false) {
            Ok(()) => {
                self.config = Config::load(&self.config_path).ok();
                self.message = Some((message, false));
                self.reload_daemon();
                self.last_status = Instant::now() - Duration::from_secs(5);
            }
            Err(err) => self.message = Some((err, true)),
        }
    }

    fn reload_daemon(&mut self) {
        match self.command_client().request(&Request::new("reload")) {
            Ok(response) if response.ok => {
                self.config = Config::load(&self.config_path).ok();
                if let Some(restart) = response
                    .data
                    .as_ref()
                    .and_then(|data| data.get("restart_required"))
                    .and_then(|value| value.as_array())
                {
                    let names: Vec<&str> =
                        restart.iter().filter_map(|value| value.as_str()).collect();
                    if !names.is_empty() {
                        self.message = Some((
                            format!("saved; restart required for: {}", names.join(", ")),
                            false,
                        ));
                    }
                }
            }
            Ok(response) => {
                self.message = Some((
                    response
                        .error
                        .unwrap_or_else(|| "reload failed".to_string()),
                    true,
                ));
            }
            Err(err) => self.message = Some((err, true)),
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        match &mut modal {
            Modal::Confirm { action, .. } => match key.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let action = std::mem::replace(action, ConfirmAction::StartService);
                    self.execute_confirm(action);
                }
                KeyCode::Char('n') | KeyCode::Esc => {}
                _ => self.modal = Some(modal),
            },
            Modal::Form(form) => {
                if key.code == KeyCode::Esc {
                    return;
                }
                if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    match self.save_form(form) {
                        Ok(message) => {
                            self.message = Some((message, false));
                            self.config = Config::load(&self.config_path).ok();
                            self.reload_daemon();
                            self.last_status = Instant::now() - Duration::from_secs(5);
                        }
                        Err(err) => {
                            form.error = Some(err);
                            self.modal = Some(modal);
                        }
                    }
                    return;
                }
                form.handle_key(key);
                self.modal = Some(modal);
            }
        }
    }

    fn save_form(&mut self, form: &Form) -> Result<String, String> {
        match &form.action {
            FormAction::AddHost | FormAction::EditHost(_) => {
                let edit = host_edit_from_form(form)?;
                let mut doc = ConfigDoc::load(&self.config_path)?;
                doc.upsert_host(&edit)?;
                doc.save(false)?;
                Ok(format!("host {} saved", edit.name))
            }
            FormAction::WanRules(interface) => {
                let edit = target_edit_from_form(form, interface)?;
                let mut doc = ConfigDoc::load(&self.config_path)?;
                doc.set_target_rules(&edit)?;
                doc.save(false)?;
                Ok(format!("rules for {interface} saved"))
            }
            FormAction::Settings => {
                apply_settings_form(form, &self.config_path)?;
                Ok("settings saved".to_string())
            }
        }
    }

    fn execute_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::RestartWan(interface) => self.send_restart(&interface, false),
            ConfirmAction::TestWan(interface) => self.send_restart(&interface, true),
            ConfirmAction::RemoveWan(interface) => {
                let Ok(mut doc) = ConfigDoc::load(&self.config_path) else {
                    return;
                };
                match doc.remove_target(&interface) {
                    Ok(()) => self.commit_doc(doc, format!("{interface} removed")),
                    Err(err) => self.message = Some((err, true)),
                }
            }
            ConfirmAction::RemoveHost(name) => {
                let Ok(mut doc) = ConfigDoc::load(&self.config_path) else {
                    return;
                };
                match doc.remove_host(&name) {
                    Ok(()) => self.commit_doc(doc, format!("host {name} removed")),
                    Err(err) => self.message = Some((err, true)),
                }
            }
            ConfirmAction::StartService => {
                let Ok(config) = Config::load(&self.config_path) else {
                    return;
                };
                match ProcessCommand::new(&config.init_script)
                    .arg("start")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                {
                    Ok(status) if status.success() => {
                        self.message = Some(("service started; reconnecting".to_string(), false))
                    }
                    Ok(status) => {
                        self.message = Some((format!("service start failed: {status}"), true))
                    }
                    Err(err) => {
                        self.message =
                            Some((format!("cannot run {}: {err}", config.init_script), true))
                    }
                }
                self.last_status = Instant::now() - Duration::from_secs(5);
            }
        }
    }

    fn send_restart(&mut self, interface: &str, test: bool) {
        match self.command_client().request(&Request::with_args(
            "pppoe-restart",
            serde_json::json!({
                "interface": interface,
                "test": test,
                "force": false,
            }),
        )) {
            Ok(response) if response.ok => {
                self.message = Some((
                    format!(
                        "{}: {}",
                        interface,
                        response
                            .data
                            .as_ref()
                            .and_then(|data| data.get("message"))
                            .and_then(|value| value.as_str())
                            .unwrap_or("restart started")
                    ),
                    false,
                ));
                self.last_status = Instant::now() - Duration::from_secs(5);
            }
            Ok(response) => {
                self.message = Some((
                    response
                        .error
                        .unwrap_or_else(|| "restart refused".to_string()),
                    true,
                ))
            }
            Err(err) => self.message = Some((err, true)),
        }
    }
}

impl ClientApp {
    fn filtered_log_count(&self) -> usize {
        self.logs
            .iter()
            .filter(|line| {
                self.log_filter
                    .map(|filter| log_level_rank(line.level) >= log_level_rank(filter))
                    .unwrap_or(true)
            })
            .count()
    }
}

fn log_level_rank(level: LogLevel) -> u8 {
    match level {
        LogLevel::Info => 0,
        LogLevel::Warn => 1,
        LogLevel::Error => 2,
    }
}

fn toggle_log_filter(current: Option<LogLevel>, level: LogLevel) -> Option<LogLevel> {
    if current == Some(level) {
        None
    } else {
        Some(level)
    }
}

fn move_index(current: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let max = count - 1;
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        (current + delta as usize).min(max)
    }
}

fn next_tab(current: ClientTab, delta: isize) -> ClientTab {
    let index = current.index() as isize + delta;
    let index = index.rem_euclid(ClientTab::ALL.len() as isize) as usize;
    ClientTab::ALL[index]
}

fn host_edit_from_form(form: &Form) -> Result<HostEdit, String> {
    let name = form.value("name").trim();
    if name.is_empty() {
        return Err("host name is required".to_string());
    }
    let ip = form.value("ip").trim();
    if ip.is_empty() {
        return Err("IPv4 address is required".to_string());
    }
    let mac = form.value("mac").trim();
    if mac.is_empty() {
        return Err("MAC address is required".to_string());
    }
    Ok(HostEdit {
        original_name: match &form.action {
            FormAction::EditHost(original) => Some(original.clone()),
            _ => None,
        },
        name: name.to_string(),
        ip: ip.to_string(),
        mac: mac.to_string(),
        enabled: parse_bool(form.value("enabled"))?,
        check_interval_secs: parse_optional_int(form.value("check_interval_secs"))?,
        wake_retry_secs: parse_optional_int(form.value("wake_retry_secs"))?,
        failures_before_wake: parse_optional_int(form.value("failures_before_wake"))?
            .map(|value| value as u32),
        max_wakes_per_hour: parse_optional_int(form.value("max_wakes_per_hour"))?
            .map(|value| value as u32),
    })
}

fn target_edit_from_form(form: &Form, interface: &str) -> Result<TargetEdit, String> {
    Ok(TargetEdit {
        interface: interface.to_string(),
        enabled: None,
        trigger: Some(parse_trigger_form(form.value("trigger"))?),
        poll_secs: Some(parse_required_int(form.value("poll_secs"), "poll_secs")?),
        down_secs: Some(parse_required_int(form.value("down_secs"), "down_secs")?),
        settle_secs: Some(parse_required_int(
            form.value("settle_secs"),
            "settle_secs",
        )?),
        retry_secs: Some(parse_required_int(form.value("retry_secs"), "retry_secs")?),
        max_restarts_per_hour: Some(parse_required_int(
            form.value("max_restarts_per_hour"),
            "max_restarts_per_hour",
        )? as u32),
        white_cidrs: Some(parse_list_form(form.value("white_cidrs"))),
        grey_cidrs: Some(parse_list_form(form.value("grey_cidrs"))),
    })
}

fn apply_settings_form(form: &Form, path: &Path) -> Result<(), String> {
    let config = Config::load(path)?;
    let mut doc = ConfigDoc::load(path)?;
    doc.set_wol_enabled(parse_bool(form.value("wol_enabled"))?);
    doc.set_top_int(
        "check_interval_secs",
        parse_required_int(form.value("check_interval_secs"), "check_interval_secs")?,
    );
    doc.set_top_int(
        "ping_timeout_ms",
        parse_required_int(form.value("ping_timeout_ms"), "ping_timeout_ms")?,
    );
    doc.set_top_int(
        "failures_before_wake",
        parse_required_int(form.value("failures_before_wake"), "failures_before_wake")?,
    );
    doc.set_top_int(
        "wake_retry_secs",
        parse_required_int(form.value("wake_retry_secs"), "wake_retry_secs")?,
    );
    doc.set_top_int(
        "max_wakes_per_hour",
        parse_required_int(form.value("max_wakes_per_hour"), "max_wakes_per_hour")?,
    );
    doc.set_top_bool("wake_on_start", parse_bool(form.value("wake_on_start"))?);
    doc.set_top_bool("verbose_probes", parse_bool(form.value("verbose_probes"))?);
    doc.set_top_int(
        "wol_port",
        parse_required_int(form.value("wol_port"), "wol_port")?,
    );
    let broadcast = form.value("broadcast").trim();
    doc.set_top_string(
        "broadcast",
        if broadcast.is_empty() {
            "auto"
        } else {
            broadcast
        },
    );
    doc.set_top_string("log_file", form.value("log_file").trim());
    doc.set_top_string("control_socket", form.value("control_socket").trim());
    doc.set_top_string("init_script", form.value("init_script").trim());
    doc.set_top_string("state_file", form.value("state_file").trim());
    let rci = RciEdit {
        url: form.value("rci.url").trim().to_string(),
        token: form.value("rci.token").trim().to_string(),
        timeout_ms: parse_required_int(form.value("rci.timeout_ms"), "rci.timeout_ms")?,
        auto_token: parse_bool(form.value("rci.auto_token"))?,
        ndmc_path: form.value("rci.ndmc_path").trim().to_string(),
    };
    doc.set_rci(&rci)?;
    doc.set_pppoe_enabled(parse_bool(form.value("pppoe.enabled"))?)?;
    let defaults = PppoeDefaults {
        trigger: parse_trigger_form(form.value("pppoe.trigger"))?,
        poll_secs: parse_required_int(form.value("pppoe.poll_secs"), "pppoe.poll_secs")?,
        down_secs: parse_required_int(form.value("pppoe.down_secs"), "pppoe.down_secs")?,
        settle_secs: parse_required_int(form.value("pppoe.settle_secs"), "pppoe.settle_secs")?,
        retry_secs: parse_required_int(form.value("pppoe.retry_secs"), "pppoe.retry_secs")?,
        max_restarts_per_hour: parse_required_int(
            form.value("pppoe.max_restarts_per_hour"),
            "pppoe.max_restarts_per_hour",
        )? as u32,
        white_cidrs: parse_list_form(form.value("pppoe.white_cidrs")),
        grey_cidrs: parse_list_form(form.value("pppoe.grey_cidrs")),
    };
    let _ = config;
    doc.set_pppoe_defaults(&defaults)?;
    doc.save(false)
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(format!("expected true/false, got {other:?}")),
    }
}

fn parse_trigger_form(value: &str) -> Result<Trigger, String> {
    match value.trim() {
        "on-change" => Ok(Trigger::OnChange),
        "always" => Ok(Trigger::Always),
        other => Err(format!(
            "trigger must be on-change or always, got {other:?}"
        )),
    }
}

fn parse_optional_int(value: &str) -> Result<Option<u64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("expected an integer or empty, got {value:?}"))
}

fn parse_required_int(value: &str, label: &str) -> Result<u64, String> {
    let value = value.trim();
    let parsed: u64 = value
        .parse()
        .map_err(|_| format!("{label} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{label} must be greater than zero"));
    }
    Ok(parsed)
}

fn parse_list_form(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn render_client(
    frame: &mut Frame,
    area: Rect,
    snapshot: Option<&Snapshot>,
    config: Option<&Config>,
    logs: &[LogLine],
    connected: bool,
    daemon_pid: Option<u64>,
    tab: ClientTab,
    host_selected: usize,
    wan_selected: usize,
    log_offset: usize,
    log_follow: bool,
    log_filter: Option<LogLevel>,
    message: Option<&(String, bool)>,
    modal: Option<&Modal>,
    now: SystemTime,
) {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }
    let header_height = u16::from(area.height >= 8) * 2;
    let footer_height = 1;
    let tabs_height = 1;
    let body_height = area
        .height
        .saturating_sub(header_height + tabs_height + footer_height);
    let header = Rect::new(area.x, area.y, area.width, header_height);
    let tabs = Rect::new(
        area.x,
        area.y.saturating_add(header_height),
        area.width,
        tabs_height,
    );
    let body = Rect::new(
        area.x,
        area.y.saturating_add(header_height + tabs_height),
        area.width,
        body_height,
    );
    let footer = Rect::new(
        area.x,
        area.y
            .saturating_add(area.height.saturating_sub(footer_height)),
        area.width,
        footer_height,
    );

    render_client_header(frame, header, snapshot, config, connected, daemon_pid);
    render_client_tabs(frame, tabs, tab);
    match tab {
        ClientTab::Overview => {
            if let Some(snapshot) = snapshot {
                if body.height >= 12 {
                    let hosts_height = body.height.saturating_mul(3) / 5;
                    let hosts_area = Rect::new(body.x, body.y, body.width, hosts_height);
                    let wan_area = Rect::new(
                        body.x,
                        body.y.saturating_add(hosts_height),
                        body.width,
                        body.height.saturating_sub(hosts_height),
                    );
                    render_hosts(
                        frame,
                        hosts_area,
                        snapshot,
                        UiState {
                            selected: host_selected,
                            host_count: snapshot.hosts.len(),
                        },
                        now,
                    );
                    render_wan(frame, wan_area, &snapshot.wan, wan_selected);
                } else {
                    render_hosts(
                        frame,
                        body,
                        snapshot,
                        UiState {
                            selected: host_selected,
                            host_count: snapshot.hosts.len(),
                        },
                        now,
                    );
                }
            } else {
                render_waiting(frame, body);
            }
        }
        ClientTab::Hosts => {
            if let Some(snapshot) = snapshot {
                render_hosts(
                    frame,
                    body,
                    snapshot,
                    UiState {
                        selected: host_selected,
                        host_count: snapshot.hosts.len(),
                    },
                    now,
                );
            } else {
                render_waiting(frame, body);
            }
        }
        ClientTab::Wan => {
            if let Some(snapshot) = snapshot {
                render_wan(frame, body, &snapshot.wan, wan_selected);
            } else {
                render_waiting(frame, body);
            }
        }
        ClientTab::Settings => render_settings_tab(frame, body, config),
        ClientTab::Logs => {
            render_client_logs(frame, body, logs, log_offset, log_follow, log_filter)
        }
    }
    render_client_footer(frame, footer, tab, connected, message);
    if let Some(modal) = modal {
        render_modal(frame, area, modal);
    }
}

fn render_client_header(
    frame: &mut Frame,
    area: Rect,
    snapshot: Option<&Snapshot>,
    config: Option<&Config>,
    connected: bool,
    daemon_pid: Option<u64>,
) {
    if area.is_empty() {
        return;
    }
    let mut line1 = vec![
        Span::styled(
            format!("wolguard v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled("daemon ", label_style()),
        if connected {
            Span::styled(
                format!("running (pid {})", daemon_pid.unwrap_or(0)),
                Style::default().fg(Color::Green),
            )
        } else {
            Span::styled("unreachable", Style::default().fg(Color::Red))
        },
        Span::raw("  "),
        Span::styled("WOL ", label_style()),
        Span::raw(
            snapshot
                .map(|snapshot| {
                    if snapshot.wol_enabled {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    }
                })
                .unwrap_or_else(|| "-".to_string()),
        ),
    ];
    if let Some(snapshot) = snapshot {
        line1.push(Span::raw("  "));
        line1.push(Span::styled("config ", label_style()));
        line1.push(Span::raw(snapshot.config_path.clone()));
    }
    let mut line2 = vec![Span::styled("PPPoE ", label_style())];
    if let Some(snapshot) = snapshot {
        if snapshot.wan.is_empty() {
            line2.push(Span::raw("-"));
        } else {
            for (index, wan) in snapshot.wan.iter().enumerate() {
                if index > 0 {
                    line2.push(Span::raw("  "));
                }
                line2.push(Span::styled(
                    wan.id.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                line2.push(Span::raw(" "));
                line2.push(Span::raw(
                    wan.address
                        .map(|address| address.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ));
                line2.push(Span::raw(" "));
                line2.push(Span::styled(
                    wan.class.map(IpClass::label).unwrap_or("-"),
                    wan_class_style(wan.class),
                ));
                if wan.hunting {
                    line2.push(Span::raw(" "));
                    line2.push(Span::styled("hunting", Style::default().fg(Color::Yellow)));
                }
            }
        }
    } else {
        line2.push(Span::raw("-"));
    }
    let lines = vec![Line::from(line1), Line::from(line2)];
    let count = usize::from(area.height.min(2));
    frame.render_widget(
        Paragraph::new(lines.into_iter().take(count).collect::<Vec<_>>()),
        area,
    );
    let _ = config;
}

fn render_client_tabs(frame: &mut Frame, area: Rect, active: ClientTab) {
    let mut spans = Vec::new();
    for (index, tab) in ClientTab::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let label = format!(" {} {} ", index + 1, tab.title());
        if *tab == active {
            spans.push(Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(label, label_style()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_waiting(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(
            "waiting for the daemon snapshot...\n\
                        press R to retry or S to start the service",
        )
        .style(label_style()),
        area,
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WanColumn {
    Index,
    Id,
    Provider,
    Ipv4,
    Class,
    State,
    Watch,
    Phase,
    Restarts,
}

impl WanColumn {
    fn label(self) -> &'static str {
        match self {
            WanColumn::Index => "#",
            WanColumn::Id => "ID",
            WanColumn::Provider => "Provider",
            WanColumn::Ipv4 => "IPv4",
            WanColumn::Class => "Class",
            WanColumn::State => "State",
            WanColumn::Watch => "Watch",
            WanColumn::Phase => "Phase",
            WanColumn::Restarts => "Restarts",
        }
    }

    fn constraint(self) -> Constraint {
        match self {
            WanColumn::Index => Constraint::Length(3),
            WanColumn::Id => Constraint::Length(12),
            WanColumn::Provider => Constraint::Length(18),
            WanColumn::Ipv4 => Constraint::Length(15),
            WanColumn::Class => Constraint::Length(5),
            WanColumn::State => Constraint::Length(10),
            WanColumn::Watch => Constraint::Length(5),
            WanColumn::Phase => Constraint::Length(12),
            WanColumn::Restarts => Constraint::Length(8),
        }
    }

    fn cell(self, index: usize, wan: &WanStatus) -> Cell<'static> {
        match self {
            WanColumn::Index => {
                Cell::from(Text::from((index + 1).to_string()).alignment(Alignment::Right))
            }
            WanColumn::Id => Cell::from(wan.id.clone()),
            WanColumn::Provider => Cell::from(truncate_text(&wan.description, 18)),
            WanColumn::Ipv4 => Cell::from(
                wan.address
                    .map(|address| address.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
            WanColumn::Class => Cell::from(wan.class.map(IpClass::label).unwrap_or("-"))
                .style(wan_class_style(wan.class)),
            WanColumn::State => Cell::from(format!("{}/{}", wan.state, wan.connected)),
            WanColumn::Watch => Cell::from(if wan.watched {
                if wan.enabled {
                    "on"
                } else {
                    "off"
                }
            } else {
                "-"
            }),
            WanColumn::Phase => Cell::from(wan.phase.clone()).style(if wan.hunting {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            }),
            WanColumn::Restarts => {
                Cell::from(Text::from(wan.restart_count.to_string()).alignment(Alignment::Right))
            }
        }
    }
}

fn wan_columns(width: u16) -> Vec<WanColumn> {
    if width >= 96 {
        vec![
            WanColumn::Index,
            WanColumn::Id,
            WanColumn::Provider,
            WanColumn::Ipv4,
            WanColumn::Class,
            WanColumn::State,
            WanColumn::Watch,
            WanColumn::Phase,
            WanColumn::Restarts,
        ]
    } else if width >= 77 {
        vec![
            WanColumn::Index,
            WanColumn::Id,
            WanColumn::Ipv4,
            WanColumn::Class,
            WanColumn::State,
            WanColumn::Watch,
            WanColumn::Phase,
            WanColumn::Restarts,
        ]
    } else if width >= 62 {
        vec![
            WanColumn::Index,
            WanColumn::Id,
            WanColumn::Ipv4,
            WanColumn::Class,
            WanColumn::State,
            WanColumn::Phase,
        ]
    } else if width >= 51 {
        vec![
            WanColumn::Index,
            WanColumn::Id,
            WanColumn::Ipv4,
            WanColumn::Class,
            WanColumn::Phase,
        ]
    } else {
        vec![
            WanColumn::Index,
            WanColumn::Id,
            WanColumn::Class,
            WanColumn::Phase,
        ]
    }
}

fn render_wan(frame: &mut Frame, area: Rect, statuses: &[WanStatus], selected: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" PPPoE ({}) ", statuses.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if statuses.is_empty() {
        frame.render_widget(
            Paragraph::new("no PPPoE interfaces; press s to rescan").style(label_style()),
            inner,
        );
        return;
    }
    let columns = wan_columns(inner.width);
    let header = Row::new(columns.iter().map(|column| Cell::from(column.label())));
    let rows = statuses
        .iter()
        .enumerate()
        .map(|(index, wan)| Row::new(columns.iter().map(|column| column.cell(index, wan))));
    let widths: Vec<Constraint> = columns.iter().map(|column| column.constraint()).collect();
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default().with_selected(if statuses.is_empty() {
        None
    } else {
        Some(selected.min(statuses.len() - 1))
    });
    frame.render_stateful_widget(table, inner, &mut state);
}

fn truncate_text(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut result: String = text.chars().take(width - 1).collect();
    result.push('…');
    result
}

fn wan_class_style(class: Option<IpClass>) -> Style {
    match class {
        Some(IpClass::White) => Style::default().fg(Color::Green),
        Some(IpClass::Grey) => Style::default().fg(Color::Yellow),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn render_settings_tab(frame: &mut Frame, area: Rect, config: Option<&Config>) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            " settings ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(config) = config else {
        frame.render_widget(
            Paragraph::new("cannot read the configuration file").style(label_style()),
            inner,
        );
        return;
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("LAN host monitoring: ", label_style()),
            Span::raw(if config.wol_enabled { "on" } else { "off" }),
        ]),
        Line::from(vec![
            Span::styled("PPPoE watchdog: ", label_style()),
            Span::raw(if config.pppoe.enabled { "on" } else { "off" }),
        ]),
        Line::from(vec![
            Span::styled("WOL policy: ", label_style()),
            Span::raw(format!(
                "every {} s, timeout {} ms, wake after {} failures, retry {} s, max {}/hour",
                config.check_interval_secs,
                config.ping_timeout_ms,
                config.failures_before_wake,
                config.wake_retry_secs,
                config.max_wakes_per_hour
            )),
        ]),
        Line::from(vec![
            Span::styled("PPPoE defaults: ", label_style()),
            Span::raw(format!(
                "trigger {}, poll {} s, down {} s, settle {} s, retry {} s, max {}/hour",
                config.pppoe.trigger.label(),
                config.pppoe.poll_secs,
                config.pppoe.down_secs,
                config.pppoe.settle_secs,
                config.pppoe.retry_secs,
                config.pppoe.max_restarts_per_hour
            )),
        ]),
        Line::from(vec![
            Span::styled("RCI: ", label_style()),
            Span::raw(format!(
                "{} (token {}, timeout {} ms)",
                config.rci.url,
                if config.rci.token.is_empty() {
                    "not set"
                } else {
                    "set"
                },
                config.rci.timeout_ms
            )),
        ]),
        Line::from(vec![
            Span::styled("Paths: ", label_style()),
            Span::raw(format!(
                "control {}, state {}, init {}",
                config.control_socket, config.state_file, config.init_script
            )),
        ]),
        Line::from(vec![
            Span::styled("Configured: ", label_style()),
            Span::raw(format!(
                "{} host(s), {} PPPoE target(s)",
                config.hosts.len(),
                config.pppoe.targets.len()
            )),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "press e to edit these settings",
            label_style(),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_client_logs(
    frame: &mut Frame,
    area: Rect,
    logs: &[LogLine],
    offset: usize,
    follow: bool,
    filter: Option<LogLevel>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            " log ",
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let filtered: Vec<&LogLine> = logs
        .iter()
        .filter(|line| {
            filter
                .map(|filter| log_level_rank(line.level) >= log_level_rank(filter))
                .unwrap_or(true)
        })
        .collect();
    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new("no log lines yet").style(label_style()),
            inner,
        );
        return;
    }
    let rows = inner.height as usize;
    let total = filtered.len();
    let max_start = total.saturating_sub(rows);
    let start = if follow {
        max_start
    } else {
        offset.min(max_start)
    };
    let end = (start + rows).min(total);
    let lines: Vec<Line> = filtered[start..end]
        .iter()
        .map(|line| {
            Line::from(vec![
                Span::styled(format_hms(line.at), label_style()),
                Span::raw(" "),
                Span::styled(line.level.label(), level_style(line.level)),
                Span::raw(" "),
                Span::raw(line.text.as_str()),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_client_footer(
    frame: &mut Frame,
    area: Rect,
    tab: ClientTab,
    connected: bool,
    message: Option<&(String, bool)>,
) {
    if area.is_empty() {
        return;
    }
    if let Some((text, error)) = message {
        let style = if *error {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Yellow)
        };
        frame.render_widget(Paragraph::new(Span::styled(text.clone(), style)), area);
        return;
    }
    if !connected {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("daemon unavailable ", Style::default().fg(Color::Red)),
                Span::styled("S", accent_style()),
                Span::raw(" start service   "),
                Span::styled("R", accent_style()),
                Span::raw(" retry   "),
                Span::styled("q", accent_style()),
                Span::raw(" quit"),
            ])),
            area,
        );
        return;
    }
    let keys = match tab {
        ClientTab::Overview => "q quit   Tab tabs   j/k select   w wake   d remove   e edit",
        ClientTab::Hosts => {
            "j/k select   w wake   Space enable/disable   a add   e edit   d remove"
        }
        ClientTab::Wan => {
            "j/k select   r restart   t test   w watch   e rules   d remove   s rescan"
        }
        ClientTab::Settings => "e edit settings   s refresh   Tab tabs",
        ClientTab::Logs => "f follow   j/k scroll   g/G top/bottom   i/W/E/A filter",
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(keys, label_style()),
            Span::raw("   "),
            Span::styled("q", accent_style()),
            Span::raw(" quit"),
        ])),
        area,
    );
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &Modal) {
    match modal {
        Modal::Confirm {
            title, question, ..
        } => {
            let width = area.width.saturating_sub(4).clamp(30, 64);
            let height = 7.min(area.height);
            let x = area.x + area.width.saturating_sub(width) / 2;
            let y = area.y + area.height.saturating_sub(height) / 2;
            let rect = Rect::new(x, y, width, height);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(Span::styled(
                    format!(" {title} "),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::raw(question.as_str())),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("y/Enter", accent_style()),
                        Span::raw(" confirm   "),
                        Span::styled("n/Esc", accent_style()),
                        Span::raw(" cancel"),
                    ]),
                ]),
                inner,
            );
        }
        Modal::Form(form) => {
            let height = (form.fields.len() as u16 + 5).min(area.height);
            let width = area.width.saturating_sub(4).clamp(40, 78);
            let x = area.x + area.width.saturating_sub(width) / 2;
            let y = area.y + area.height.saturating_sub(height) / 2;
            let rect = Rect::new(x, y, width, height);
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(Span::styled(
                    format!(" {} ", form.title),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let mut lines = Vec::new();
            let visible = inner.height.saturating_sub(3) as usize;
            let start = if form.fields.len() > visible && visible > 0 {
                form.selected
                    .saturating_sub(visible / 2)
                    .min(form.fields.len() - visible)
            } else {
                0
            };
            for (index, field) in form
                .fields
                .iter()
                .enumerate()
                .skip(start)
                .take(if visible == 0 {
                    form.fields.len()
                } else {
                    visible
                })
            {
                let selected = index == form.selected;
                let hint = match field.kind {
                    FieldKind::Bool => " (Space toggles)",
                    FieldKind::Trigger => " (Space cycles)",
                    FieldKind::Int => " (empty = inherit)",
                    _ => "",
                };
                let mut spans = vec![
                    Span::styled(
                        format!("{:<24}", truncate_text(&field.label, 24)),
                        if selected {
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                        } else {
                            label_style()
                        },
                    ),
                    Span::raw(if field.value.is_empty() {
                        " ".to_string()
                    } else {
                        field.value.clone()
                    }),
                    Span::styled(hint, label_style()),
                ];
                if selected {
                    spans.insert(0, Span::styled("› ", accent_style()));
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Ctrl-S", accent_style()),
                Span::raw(" save   "),
                Span::styled("Enter/Space", accent_style()),
                Span::raw(" next/toggle   "),
                Span::styled("Esc", accent_style()),
                Span::raw(" cancel"),
            ]));
            if let Some(error) = &form.error {
                lines.push(Line::from(Span::styled(
                    error.clone(),
                    Style::default().fg(Color::Red),
                )));
            }
            frame.render_widget(Paragraph::new(lines), inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::UNIX_EPOCH;

    use ratatui::backend::TestBackend;

    fn sample_host(name: &str, state: HostState, enabled: bool) -> HostStatus {
        HostStatus {
            name: name.to_string(),
            ip: Ipv4Addr::new(192, 168, 1, 10),
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            enabled,
            state,
            consecutive_failures: 2,
            last_rtt: Some(Duration::from_micros(1_250)),
            last_ok: Some(UNIX_EPOCH + Duration::from_secs(9_990)),
            last_check: Some(UNIX_EPOCH + Duration::from_secs(9_995)),
            last_wake: Some(UNIX_EPOCH + Duration::from_secs(9_000)),
            wake_count: 3,
        }
    }

    fn sample_snapshot() -> Snapshot {
        Snapshot {
            hosts: vec![
                sample_host("router", HostState::Online, true),
                sample_host("kitchen-laptop", HostState::Offline, true),
                sample_host("café-workstation", HostState::Waking, true),
                sample_host("unknown-camera", HostState::Unknown, true),
                sample_host(
                    "disabled-printer-with-a-very-long-host-name",
                    HostState::Disabled,
                    false,
                ),
            ],
            started_at: UNIX_EPOCH + Duration::from_secs(1_000),
            config_path: "/opt/etc/wolguard.conf".to_string(),
            wol_port: 9,
            wake_target: "192.168.1.255 (br0)".to_string(),
            last_update: UNIX_EPOCH + Duration::from_secs(9_999),
            wol_enabled: true,
            wan: Vec::new(),
            session_signals: Vec::new(),
        }
    }

    fn sample_logs(now: SystemTime) -> Vec<LogLine> {
        vec![
            LogLine {
                seq: 1,
                at: now,
                level: LogLevel::Info,
                text: "kitchen-laptop (192.168.1.10) is online".to_string(),
            },
            LogLine {
                seq: 2,
                at: now,
                level: LogLevel::Warn,
                text: "café-workstation: wake rate limit reached".to_string(),
            },
            LogLine {
                seq: 3,
                at: now,
                level: LogLevel::Error,
                text: "a deliberately long error message that must be truncated to the terminal \
                       width without panicking on unicode boundaries — café naïve façade"
                    .to_string(),
            },
        ]
    }

    fn draw_at(width: u16, height: u16) {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let snapshot = sample_snapshot();
        let logs = sample_logs(now);
        let ui = UiState {
            selected: 1,
            host_count: snapshot.hosts.len(),
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &snapshot, &logs, ui, now))
            .expect("frame must render");
    }

    #[test]
    fn layout_regions_stay_inside_the_terminal() {
        for width in [0, 1, 10, 40, 80, 200] {
            for height in [0, 1, 4, 8, 10, 24, 60] {
                let area = Rect::new(0, 0, width, height);
                let areas = layout_areas(area);
                for region in [areas.header, areas.table, areas.log, areas.footer] {
                    assert!(
                        region.right() <= width && region.bottom() <= height,
                        "{width}x{height}: {region:?} escapes the terminal"
                    );
                }
                let total = areas.header.height
                    + areas.table.height
                    + areas.log.height
                    + areas.footer.height;
                assert_eq!(total, height, "{width}x{height}: rows must add up");
            }
        }
    }

    #[test]
    fn renders_tiny_narrow_and_unicode_frames_without_panicking() {
        // Below the minimum size the dashboard only shows a resize hint, and it
        // must not panic there either.
        for (width, height) in [
            (1, 1),
            (20, 8),
            (39, 9),
            (40, 10),
            (48, 12),
            (59, 16),
            (77, 20),
            (84, 24),
            (95, 30),
            (120, 40),
            (200, 60),
        ] {
            draw_at(width, height);
        }
    }

    #[test]
    fn empty_host_list_keeps_the_table_and_shows_a_hint() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let mut snapshot = sample_snapshot();
        snapshot.hosts.clear();
        let ui = UiState::default();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &snapshot, &[], ui, now))
            .expect("frame must render");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("hosts (0)"),
            "table title must show the empty count: {text:?}"
        );
        assert!(
            text.contains("no hosts configured"),
            "empty state must tell the user what to do: {text:?}"
        );
    }

    #[test]
    fn state_and_log_level_palette_matches_the_documented_colors() {
        let states = [
            (HostState::Online, Color::Green),
            (HostState::Offline, Color::Red),
            (HostState::Waking, Color::Yellow),
            (HostState::Unknown, Color::Gray),
            (HostState::Disabled, Color::DarkGray),
        ];
        for (state, color) in states {
            assert_eq!(state_style(state).fg, Some(color), "{state:?}");
        }
        // Info stays neutral, warnings and errors carry the only other colors.
        assert_eq!(level_style(LogLevel::Info).fg, None);
        assert_eq!(level_style(LogLevel::Warn).fg, Some(Color::Yellow));
        assert_eq!(level_style(LogLevel::Error).fg, Some(Color::Red));
    }

    #[test]
    fn narrow_terminals_drop_optional_columns_in_canonical_order() {
        let labels = |width: u16| -> Vec<&'static str> {
            columns(width).iter().map(|column| column.label()).collect()
        };
        assert_eq!(labels(20), vec!["#", "Name", "IP", "State"]);
        assert_eq!(labels(48), vec!["#", "Name", "IP", "State", "RTT"]);
        assert_eq!(
            labels(59),
            vec!["#", "Name", "IP", "State", "RTT", "Last seen"]
        );
        assert_eq!(
            labels(77),
            vec!["#", "Name", "IP", "MAC", "State", "RTT", "Last seen"]
        );
        assert_eq!(
            labels(84),
            vec![
                "#",
                "Name",
                "IP",
                "MAC",
                "State",
                "RTT",
                "Last seen",
                "Wakes"
            ]
        );
        assert_eq!(
            labels(95),
            vec![
                "#",
                "Name",
                "IP",
                "MAC",
                "State",
                "RTT",
                "Last seen",
                "Wakes",
                "Last wake"
            ]
        );
    }

    fn sample_wan() -> WanStatus {
        WanStatus {
            id: "PPPoE0".to_string(),
            description: "Provider".to_string(),
            interface_name: "ppp0".to_string(),
            state: "up".to_string(),
            link: "up".to_string(),
            connected: "yes".to_string(),
            address: Some(Ipv4Addr::new(100, 70, 12, 34)),
            mask: Some(Ipv4Addr::new(255, 255, 255, 255)),
            class: Some(crate::ipclass::IpClass::Grey),
            class_reason: Some("CGNAT 100.64.0.0/10".to_string()),
            uptime_secs: Some(120),
            watched: true,
            enabled: true,
            hunting: true,
            phase: "waiting-ip".to_string(),
            restart_count: 2,
            last_restart: Some(UNIX_EPOCH + Duration::from_secs(9_900)),
            next_action_in_secs: Some(5),
            last_error: None,
            recovery_pending: false,
        }
    }

    #[test]
    fn client_wan_table_renders_at_multiple_sizes() {
        let statuses = vec![sample_wan()];
        for (width, height) in [(40, 10), (80, 24), (120, 40)] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            terminal
                .draw(|frame| {
                    render_wan(frame, frame.area(), &statuses, 0);
                })
                .expect("WAN table must render");
        }
    }

    #[test]
    fn client_modals_and_settings_render() {
        let config = crate::config::Config::from_toml_str(
            r#"
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
[[pppoe.target]]
interface = "PPPoE0"
"#,
        )
        .expect("sample config");
        for (width, height) in [(40, 10), (80, 24), (120, 40)] {
            let confirm = Modal::Confirm {
                title: "Restart".to_string(),
                question: "Restart PPPoE0?".to_string(),
                action: ConfirmAction::StartService,
            };
            let form = Form::settings_form(&config);
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            terminal
                .draw(|frame| {
                    render_settings_tab(frame, frame.area(), Some(&config));
                    render_modal(frame, frame.area(), &confirm);
                    render_modal(frame, frame.area(), &Modal::Form(form));
                })
                .expect("modals must render");
        }
    }

    #[test]
    fn wan_columns_drop_secondary_fields_on_narrow_terminals() {
        let labels = |width: u16| -> Vec<&'static str> {
            wan_columns(width)
                .iter()
                .map(|column| column.label())
                .collect()
        };
        assert_eq!(
            labels(96),
            vec!["#", "ID", "Provider", "IPv4", "Class", "State", "Watch", "Phase", "Restarts"]
        );
        assert_eq!(
            labels(77),
            vec!["#", "ID", "IPv4", "Class", "State", "Watch", "Phase", "Restarts"]
        );
        assert_eq!(
            labels(70),
            vec!["#", "ID", "IPv4", "Class", "State", "Phase"]
        );
        assert_eq!(labels(60), vec!["#", "ID", "IPv4", "Class", "Phase"]);
        assert_eq!(labels(40), vec!["#", "ID", "Class", "Phase"]);
    }

    #[test]
    fn client_full_frame_renders_all_tabs() {
        let mut snapshot = sample_snapshot();
        snapshot.wan = vec![sample_wan()];
        snapshot.session_signals = vec!["address".to_string(), "uptime".to_string()];
        let config = crate::config::Config::from_toml_str(
            r#"
[[host]]
name = "server"
ip = "192.168.1.10"
mac = "AA:BB:CC:DD:EE:FF"
[[pppoe.target]]
interface = "PPPoE0"
"#,
        )
        .expect("sample config");
        let logs = sample_logs(SystemTime::now());
        for tab in ClientTab::ALL {
            for (width, height) in [(40, 10), (80, 24), (120, 40)] {
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("test terminal");
                terminal
                    .draw(|frame| {
                        render_client(
                            frame,
                            frame.area(),
                            Some(&snapshot),
                            Some(&config),
                            &logs,
                            true,
                            Some(123),
                            tab,
                            0,
                            0,
                            0,
                            true,
                            None,
                            None,
                            None,
                            UNIX_EPOCH + Duration::from_secs(10_000),
                        );
                    })
                    .expect("client frame must render");
            }
        }
    }
}
