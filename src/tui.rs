//! Interactive terminal dashboard (ratatui).
//!
//! The dashboard owns the terminal (alternate screen, raw mode, hidden cursor)
//! while it runs and restores it on every exit path. It only reads the shared
//! snapshot and log buffer, and talks to the monitor thread through the command
//! channel, so a slow terminal never blocks monitoring.

use std::io::{self, IsTerminal};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{Frame, Terminal};

use crate::model::{
    Command, HostState, HostStatus, LogLevel, LogLine, SharedLogs, SharedSnapshot, Snapshot,
};
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

/// Run the dashboard until the user quits or a stop signal arrives.
pub fn run(
    snapshot: SharedSnapshot,
    logs: SharedLogs,
    commands: Sender<Command>,
) -> io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "wolguard tui requires a terminal (stdin and stdout must be a TTY)",
        ));
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
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(err);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(TerminalSession { terminal }),
            Err(err) => {
                restore_terminal();
                Err(err)
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
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
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
            if key.kind != KeyEventKind::Release && handle_key(key, ui, commands) {
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
fn handle_key(key: KeyEvent, ui: &mut UiState, commands: &Sender<Command>) -> bool {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up | KeyCode::Char('k') => ui.select_previous(),
        KeyCode::Down | KeyCode::Char('j') => ui.select_next(),
        KeyCode::Char('w') => {
            if let Some(index) = ui.selected_index() {
                // The monitor may already be gone; the dashboard keeps showing
                // the last snapshot either way.
                let _ = commands.send(Command::WakeNow(index));
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
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
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
        }
    }

    fn sample_logs(now: SystemTime) -> Vec<LogLine> {
        vec![
            LogLine {
                at: now,
                level: LogLevel::Info,
                text: "kitchen-laptop (192.168.1.10) is online".to_string(),
            },
            LogLine {
                at: now,
                level: LogLevel::Warn,
                text: "café-workstation: wake rate limit reached".to_string(),
            },
            LogLine {
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
}
